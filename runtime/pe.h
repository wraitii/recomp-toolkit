#pragma once
#include <cstddef>
#include <cstdint>
#include <string>
#include <vector>

struct SectionInfo {
    std::string name;
    uint32_t va;       // guest virtual address
    uint32_t vsize;    // Misc_VirtualSize
    uint32_t raw_size; // SizeOfRawData
    uint32_t raw_ptr;  // PointerToRawData
    uint32_t characteristics;
};

struct PeLayout {
    size_t fh_off = 0, opt_off = 0, sh_off = 0;
    uint16_t opt_size = 0, nsections = 0, machine = 0, magic = 0;
    uint32_t size_image = 0, size_hdrs = 0;
};

bool pe_main_layout(const std::vector<uint8_t> &file, PeLayout &pe, std::string &error);
bool pe_layout(const std::vector<uint8_t> &file, const std::string &path, PeLayout &pe,
               std::string &error);
bool pe_copy(const std::vector<uint8_t> &file, const PeLayout &pe, const std::string &name,
             uint32_t base, std::vector<SectionInfo> *sections, std::string &error);
bool pe_relocate(const char *name, uint32_t base, uint32_t preferred, uint32_t size,
                 uint32_t reloc_rva, uint32_t reloc_size, std::string &error);

struct PeImport {
    uint32_t slot, width, ordinal;
    std::string name;
    bool by_ordinal;
};
struct PeImportModule {
    std::string dll;
    std::vector<PeImport> imports;
};
bool pe_imports(const std::vector<uint8_t> &file, size_t opt_off, uint16_t magic, uint32_t base,
                uint32_t size, bool strict, bool require_directory,
                std::vector<PeImportModule> &modules, std::string &error);

struct PeExport {
    uint32_t address = 0;
    std::string forwarder;
};
PeExport pe_export_name(uint32_t base, uint32_t size, uint32_t export_rva, uint32_t export_size,
                        const char *name);
PeExport pe_export_ordinal(uint32_t base, uint32_t size, uint32_t export_rva, uint32_t export_size,
                           uint32_t ordinal);

struct PeTls {
    uint32_t raw_start = 0, raw_end = 0, index_addr = 0, callbacks = 0, zero_fill = 0;
};
bool pe_tls(const std::vector<uint8_t> &file, const PeLayout &pe, uint32_t base, PeTls &tls,
            std::string &error);

bool pe_directory(uint32_t base, uint32_t image_size, uint32_t index, uint32_t &rva,
                  uint32_t &size);

struct ResourceName {
    bool is_string = false;
    uint32_t id = 0;
    std::string name; // UTF-8, decoded from the directory's length-prefixed UTF-16.
};

uint32_t pe_resource_find(uint32_t base, uint32_t size, const ResourceName &type,
                          const ResourceName &name, std::string *why);
uint32_t pe_resource_data(uint32_t base, uint32_t image_size, uint32_t entry, uint32_t *size,
                          std::string *why);
bool pe_resource_names(uint32_t base, uint32_t size, const ResourceName &type,
                       std::vector<ResourceName> *names, std::string *why);

bool pe_forwarder(const std::string &forwarder, std::string &module, std::string &name,
                  uint32_t &ordinal, bool &by_ordinal);
