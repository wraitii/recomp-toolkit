#pragma once
#include "vfs.h"

void win_path_init(const std::string &game_dir);
const std::string &win_path_current();
void win_path_set_current(const std::string &path);
VfsPath win_path_components(const std::string &path);
