#include "resources.h"
#include "loader.h"
#include "../platform/os.h"

namespace {
bool fail(std::string *why, const char *message) {
    if (why)
        *why = message;
    return false;
}

bool identifier(uint32_t p, ResourceName &out, std::string *why, bool wide = true) {
    out = {};
    if (p <= 0xffff) {
        out.id = p;
        return true;
    }
    if (!gm_valid(p, wide ? 2 : 1))
        return fail(why, "invalid resource identifier address");
    std::string text = wide ? gm_wstr(p) : gm_str(p);
    if (!text.empty() && text[0] == '#') {
        if (text.size() == 1)
            return fail(why, "empty numeric resource identifier");
        uint32_t id = 0;
        for (size_t i = 1; i < text.size(); ++i) {
            if (text[i] < '0' || text[i] > '9' || id > 6553)
                return fail(why, "invalid numeric resource identifier");
            id = id * 10 + (uint32_t)(text[i] - '0');
            if (id > 0xffff)
                return fail(why, "resource identifier exceeds 16 bits");
        }
        out.id = id;
    } else {
        out.is_string = true;
        out.name = text;
    }
    return true;
}
} // namespace

namespace {
bool resource_image(uint32_t module, uint32_t &base, uint32_t &size, std::string *why) {
    if (why)
        why->clear();
    const LoaderModule *owner = module ? loader_module_containing(module) : nullptr;
    if (module && !owner)
        owner = loader_datafile(module);
    if (module && (!owner || owner->base != module))
        return fail(why, "invalid resource module");
    base = owner ? owner->base : loader_image_base();
    size = owner ? owner->size : loader_image_size();
    return true;
}
} // namespace

uint32_t resource_find(uint32_t type_id_or_name, uint32_t name_id_or_name, std::string *why,
                       uint32_t module) {
    uint32_t base, size;
    ResourceName type, name;
    if (!resource_image(module, base, size, why) || !identifier(type_id_or_name, type, why) ||
        !identifier(name_id_or_name, name, why))
        return 0;
    return pe_resource_find(base, size, type, name, why);
}
uint32_t resource_data(uint32_t entry, uint32_t *size, std::string *why, uint32_t module) {
    if (size)
        *size = 0;
    uint32_t base, image_size;
    return resource_image(module, base, image_size, why)
               ? pe_resource_data(base, image_size, entry, size, why)
               : 0;
}
bool resource_names(uint32_t type_id_or_name, std::vector<ResourceName> *names, std::string *why,
                    uint32_t module, bool wide_type) {
    if (!names)
        return fail(why, "missing resource-name output");
    names->clear();
    uint32_t base, size;
    ResourceName type;
    if (!resource_image(module, base, size, why) ||
        !identifier(type_id_or_name, type, why, wide_type))
        return false;
    return pe_resource_names(base, size, type, names, why);
}
