#include "installation_manifest.h"

#include "../host/controls/json.h"
#include "../platform/os.h"
#include "loader.h"
#include "game_config.h"

#include <algorithm>
#include <cerrno>
#include <cstring>
#include <fstream>
#include <sstream>

namespace {

struct AuxSpec {
    const char *name, *path, *sha256;
    uint32_t base, size;
    const char *aliases;
};
const AuxSpec expected_aux[] = RECOMP_AUX_MODULES;

std::string parent(const std::string &path) {
    const size_t slash = path.find_last_of("/\\");
    return slash == std::string::npos ? std::string() : path.substr(0, slash);
}

bool safe_profile(const std::string &value) {
    if (value.empty() || value.size() > 64)
        return false;
    for (unsigned char c : value)
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
              c == '-' || c == '_' || c == ' '))
            return false;
    return true;
}

bool same_path_case_insensitive(const std::string &left, const std::string &right) {
    if (left.size() != right.size())
        return false;
    for (size_t i = 0; i < left.size(); ++i) {
        unsigned char a = (unsigned char)left[i];
        unsigned char b = (unsigned char)right[i];
        if (a >= 'A' && a <= 'Z')
            a += 'a' - 'A';
        if (b >= 'A' && b <= 'Z')
            b += 'a' - 'A';
        if (a != b)
            return false;
    }
    return true;
}

} // namespace

InstallationManifest installation_manifest_location_for_process() {
    InstallationManifest result;
    char executable[4096];
    if (os_exe_path(executable, sizeof executable) != 0)
        return result;
    std::string process_path = executable;
    for (char &c : process_path)
        if (c == '\\')
            c = '/';
    std::string folder;
    if (strcmp(os_platform_name(), "macos") == 0) {
        const std::string bundle = parent(parent(parent(process_path)));
        if (bundle.size() < 5 || bundle.compare(bundle.size() - 4, 4, ".app") != 0)
            return result;
        folder = parent(bundle);
    } else {
        const std::string directory = parent(process_path);
        if (directory.size() < 4 || directory.compare(directory.size() - 4, 4, "/app") != 0)
            return result;
        folder = parent(directory);
    }
    const std::string metadata_dir = folder + "/.recomp-toolkit";
    const std::string candidate = folder + "/.recomp-toolkit/installation.json";
    OsStat metadata{}, st{};
    const int metadata_status = os_lstat(metadata_dir.c_str(), &metadata);
    if (metadata_status == 0 && (!metadata.is_dir || metadata.is_symlink)) {
        result.found = true;
        result.game_folder = folder;
        result.path = candidate;
        result.error = "metadata directory is not a real directory";
    } else if (metadata_status == 0 && os_lstat(candidate.c_str(), &st) == 0) {
        result.game_folder = folder;
        result.path = candidate;
        result.found = true;
    } else if ((metadata_status != 0 && errno != ENOENT && errno != ENOTDIR) ||
               (metadata_status == 0 && errno != ENOENT && errno != ENOTDIR)) {
        result.found = true;
        result.game_folder = folder;
        result.path = candidate;
        result.error = "manifest cannot be inspected";
    }
    return result;
}

InstallationManifest installation_manifest_for_process() {
    InstallationManifest result = installation_manifest_location_for_process();
    if (!result.found || !result.error.empty())
        return result;
    OsStat st{};
    if (os_lstat(result.path.c_str(), &st) != 0) {
        result.error = "manifest cannot be inspected";
        return result;
    }
    if (!st.is_regular || st.size > 1024 * 1024) {
        result.error = "manifest is not a regular file within the 1 MiB limit";
        return result;
    }
    std::ifstream input(result.path, std::ios::binary);
    if (!input) {
        result.error = "manifest cannot be opened";
        return result;
    }
    std::string text;
    char buffer[4096];
    while (input) {
        input.read(buffer, sizeof buffer);
        const std::streamsize count = input.gcount();
        if (count > 0)
            text.append(buffer, (size_t)count);
        if (text.size() > 1024 * 1024) {
            result.error = "manifest exceeded the 1 MiB read limit";
            return result;
        }
    }
    controls::Json root;
    std::string parse_error;
    if (input.bad() || !controls::json_parse(text, &root, &parse_error) ||
        root.type != controls::Json::Object) {
        result.error = "manifest JSON is unreadable: " + parse_error;
        return result;
    }
    const controls::Json *schema = root.get("schema");
    const controls::Json *paths = root.get("paths");
    result.game_id = root.str("gameId", "");
    result.profile_id = root.str("profileId", "");
    result.executable = paths ? paths->str("executable", "") : "";
    result.data_root = paths ? paths->str("dataRoot", "") : "";
    const controls::Json *profile = root.get("profile");
    result.profile_suffix = profile ? profile->str("relativeRoot", "") : "";
    const controls::Json *fingerprints = root.get("fingerprints");
    const controls::Json *launch_paths = root.get("launchPaths");
    const controls::Json *executable_fingerprint =
        fingerprints ? fingerprints->get("executable") : nullptr;
    const uint64_t declared_size =
        executable_fingerprint ? (uint64_t)executable_fingerprint->num("size", 0) : 0;
    const std::string declared_hash =
        executable_fingerprint ? executable_fingerprint->str("sha256", "") : "";
    if (!schema || schema->type != controls::Json::Object || schema->num("major", 0) != 1 ||
        !paths || paths->type != controls::Json::Object || result.game_id != RECOMP_GAME_ID ||
        result.game_id.find_first_of("/\\:") != std::string::npos ||
        !safe_profile(result.profile_id) ||
        !installation_manifest_detail::safe_relative(result.executable) ||
        !installation_manifest_detail::safe_relative(result.data_root) ||
        (!result.profile_suffix.empty() &&
         !installation_manifest_detail::safe_relative(result.profile_suffix)) ||
        !executable_fingerprint || executable_fingerprint->type != controls::Json::Object ||
        !launch_paths || launch_paths->type != controls::Json::Object || declared_size == 0 ||
        declared_hash != LOADER_EXPECTED_SHA256) {
        result.error = "manifest schema, profile identity, or relative paths failed validation";
        return result;
    }
    const std::string expected_game =
        strcmp(os_platform_name(), "macos") == 0
            ? std::string(RECOMP_APP_NAME) + ".app/Contents/MacOS/" + RECOMP_APP_NAME
            : std::string("app/") + RECOMP_APP_NAME +
                  (strcmp(os_platform_name(), "windows") == 0 ? ".exe" : "");
    std::string sidecar_name = RECOMP_GAME_NAME;
    sidecar_name.erase(std::remove(sidecar_name.begin(), sidecar_name.end(), ':'),
                       sidecar_name.end());
    const std::string expected_configure =
        "Configure " + sidecar_name + ".app/Contents/MacOS/ConfigureLauncher";
    if (launch_paths->str("game", "") != expected_game ||
        launch_paths->str("configure", "") != expected_configure) {
        result.error = "manifest launch roles differ from this build";
        return result;
    }
    char profile_base[4096];
    if (os_user_data_dir(result.profile_id.c_str(), profile_base, sizeof profile_base) != 0) {
        result.error = "platform profile directory cannot be resolved";
        return result;
    }
    char canonical_profile_base[4096];
    std::string existing_profile_parent = profile_base;
    while (os_realpath(existing_profile_parent.c_str(), canonical_profile_base,
                       sizeof canonical_profile_base) != 0) {
        const std::string next = parent(existing_profile_parent);
        if (next.empty() || next == existing_profile_parent) {
            result.error = "platform profile directory cannot be canonicalized";
            return result;
        }
        existing_profile_parent = next;
    }
    std::string profile_path = profile_base;
    if (!result.profile_suffix.empty()) {
        std::stringstream suffix(result.profile_suffix);
        std::string component;
        while (std::getline(suffix, component, '/')) {
            profile_path += "/" + component;
            OsStat profile_stat{};
            if (os_lstat(profile_path.c_str(), &profile_stat) == 0 && profile_stat.is_symlink) {
                result.error = "profile relativeRoot traverses a symbolic link";
                return result;
            }
        }
    }
    char resolved_profile[4096];
    if (os_realpath(profile_path.c_str(), resolved_profile, sizeof resolved_profile) == 0) {
        const size_t profile_base_length = strlen(canonical_profile_base);
        if (strncmp(canonical_profile_base, resolved_profile, profile_base_length) != 0 ||
            (resolved_profile[profile_base_length] != '/' &&
             resolved_profile[profile_base_length] != '\0')) {
            result.error = "profile relativeRoot escapes the platform profile directory";
            return result;
        }
    }
    char base[4096], image_path[4096], data_path[4096];
    const std::string declared_image = result.game_folder + "/" + result.executable;
    const std::string declared_data = result.game_folder + "/" + result.data_root;
    if (os_realpath(result.game_folder.c_str(), base, sizeof base) != 0 ||
        os_realpath(declared_image.c_str(), image_path, sizeof image_path) != 0 ||
        os_realpath(declared_data.c_str(), data_path, sizeof data_path) != 0) {
        result.error = "manifest root, executable, or data directory cannot be resolved";
        return result;
    }
    if (!installation_manifest_detail::path_within(base, image_path) ||
        !installation_manifest_detail::path_within(base, data_path)) {
        result.error = "manifest paths escape the game folder after resolving symbolic links";
        return result;
    }
    OsStat image{}, data{};
    if (os_stat(image_path, &image) != 0 || !image.is_regular || image.size != declared_size) {
        result.error = "manifest executable is missing, changed size, or is not a regular: " +
                       std::string(image_path);
        return result;
    }
    if (os_stat(data_path, &data) != 0 || !data.is_dir) {
        result.error =
            "manifest data root is missing or is not a directory: " + std::string(data_path);
        return result;
    }
    const char *required_directories[] = RECOMP_REQUIRED_DIRS;
    for (const char *required : required_directories) {
        if (!required || !*required)
            break;
        if (!os_directory_contains(data_path, required)) {
            result.error = "installed data root is missing required directory " +
                           std::string(required) + ": " + data_path;
            return result;
        }
    }
    const std::string digest = loader_hash_file(image_path);
    if (digest.empty() || digest != LOADER_EXPECTED_SHA256) {
        result.error = "manifest executable does not match the supported image fingerprint: " +
                       std::string(image_path);
        return result;
    }
    const controls::Json *declared_aux = fingerprints->get("auxiliaries");
    if (!declared_aux || declared_aux->type != controls::Json::Array ||
        declared_aux->a.size() != RECOMP_AUX_MODULE_COUNT) {
        result.error = "manifest auxiliary fingerprints do not match the compiled module set";
        return result;
    }
    for (size_t i = 0; i < declared_aux->a.size(); ++i) {
        const controls::Json &entry = declared_aux->a[i];
        const std::string relative = entry.str("path", "");
        const std::string hash = entry.str("sha256", "");
        const uint64_t size = (uint64_t)entry.num("size", 0);
        if (entry.type != controls::Json::Object || entry.str("name", "") != expected_aux[i].name ||
            !installation_manifest_detail::safe_relative(relative) ||
            hash != expected_aux[i].sha256 || size == 0) {
            result.error = "manifest auxiliary fingerprint differs from compiled expectations";
            return result;
        }
        const std::string executable_parent = parent(result.executable);
        const std::string beside_executable =
            (executable_parent.empty() ? "" : executable_parent + "/") + expected_aux[i].name;
        const std::string configured_path = std::string(RECOMP_DEVELOPER_GAME_DIR) + "/";
        std::string configured_relative = expected_aux[i].path;
        if (configured_relative.compare(0, configured_path.size(), configured_path) == 0)
            configured_relative = (executable_parent.empty() ? "" : executable_parent + "/") +
                                  configured_relative.substr(configured_path.size());
        else
            configured_relative.clear();
        if (!same_path_case_insensitive(relative, beside_executable) &&
            !same_path_case_insensitive(relative, configured_relative)) {
            result.error = "manifest auxiliary path differs from loader resolution: " + relative;
            return result;
        }
        char auxiliary_path[4096];
        const std::string declared_auxiliary = result.game_folder + "/" + relative;
        OsStat auxiliary{};
        if (os_realpath(declared_auxiliary.c_str(), auxiliary_path, sizeof auxiliary_path) != 0 ||
            !installation_manifest_detail::path_within(base, auxiliary_path) ||
            os_stat(auxiliary_path, &auxiliary) != 0 || !auxiliary.is_regular ||
            auxiliary.size != size || loader_hash_file(auxiliary_path) != hash) {
            result.error =
                "manifest auxiliary file failed its path, size, or hash check: " + relative;
            return result;
        }
    }
    result.valid = true;
    return result;
}
