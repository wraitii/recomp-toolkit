// Shared reader for the loaded PE image's resource directory. Returned handles
// and payloads are guest addresses. No module or host pointer is substituted.
#pragma once
#include "pe.h"
#include <cstdint>
#include <string>
#include <vector>

// `module` is a mapped image base: 0 for the EXE, an auxiliary module, or a data file.
// Type and name accept integer IDs, UTF-16 "#123", or UTF-16 names. Select the
// first language and return its IMAGE_RESOURCE_DATA_ENTRY address, or zero.
uint32_t resource_find(uint32_t type_id_or_name, uint32_t name_id_or_name,
                       std::string *why = nullptr, uint32_t module = 0);
// Validate a data-entry handle and return its image-backed payload and size.
uint32_t resource_data(uint32_t entry, uint32_t *size, std::string *why = nullptr,
                       uint32_t module = 0);
// Snapshot the names before guest callbacks run; no references into the directory
// or temporary guest buffers escape this reader. False explains failure in why.
// `wide_type` selects the encoding of a string type identifier at a guest address.
bool resource_names(uint32_t type_id_or_name, std::vector<ResourceName> *names,
                    std::string *why = nullptr, uint32_t module = 0, bool wide_type = true);
