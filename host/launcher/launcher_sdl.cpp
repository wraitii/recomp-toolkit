// launcher_sdl.cpp - see launcher_sdl.h.
#include "launcher_sdl.h"

#include "../../platform/os.h"
#include "../gpu/gpu.h"
#include "../controls/controls_host.h"
#include "../controls/gamepad_sdl.h"
#include "../controls/pad_mapper_editor.h"
#include "../input_touch.h"
#include "../preferences.h"
#include "../sdl/keymap.h"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <vector>
#include <sstream>

namespace launcher {

bool requested(int argc, char **argv) {
    for (int i = 1; i < argc; ++i)
        if (strcmp(argv[i], "--launcher") == 0)
            return true;
    const char *env = recomp_env("LAUNCHER");
    if (env && *env && strcmp(env, "0") != 0)
        return true;
    SDL_PumpEvents();
    return (SDL_GetModState() & (SDL_KMOD_SHIFT | SDL_KMOD_ALT)) != 0;
}

namespace {

std::string controller_description() {
    return controls::gamepad_name() + (controls::gamepad_rumble_supported()
                                           ? " | Rumble supported"
                                           : " | Rumble not reported");
}

void draw_controller_page(Canvas &canvas, const controls::ControlsView &view) {
    const Color background{11, 21, 48, 255}, panel{20, 37, 68, 255};
    const Color button{24, 46, 80, 255}, selected{21, 85, 195, 255};
    const Color border{48, 73, 108, 255}, focus{140, 232, 255, 255};
    const Color text{237, 249, 255, 255}, dim{173, 190, 214, 255};
    auto rect = [](const controls::Rect &r) { return Rect{r.x, r.y, r.w, r.h}; };
    auto draw_text = [&](const controls::DrawText &t) {
        const auto lines = Canvas::wrap(t.text, t.rect.w, t.scale);
        int y = t.rect.y;
        for (const auto &line : lines) {
            if (y + Canvas::text_height(t.scale) > t.rect.y + t.rect.h)
                break;
            canvas.text(t.rect.x, y, line, t.scale, t.muted ? dim : text);
            y += 11 * t.scale;
        }
    };
    auto draw_key = [&](const controls::DrawControl &d) {
        const Rect r = rect(d.rect);
        canvas.fill(r, d.lit ? selected : button);
        canvas.frame(r, 1, d.focused ? focus : border);
        int scale = std::max(1, d.text_scale);
        std::string label = d.label;
        while (!label.empty() && Canvas::text_width(label, scale) > r.w - 8)
            label.pop_back();
        canvas.text(r.x + (r.w - Canvas::text_width(label, scale)) / 2,
                    r.y + (r.h - Canvas::text_height(scale)) / 2, label, scale, text);
    };
    canvas.clear(background);
    for (const auto &m : view.meters) {
        const Rect r = rect(m.rect);
        canvas.fill(r, panel);
        canvas.frame(r, 1, border);
        const double v = std::clamp(m.value, m.bipolar ? -1.0 : 0.0, 1.0);
        if (m.bipolar) {
            const int centre = r.x + r.w / 2;
            const int length = int((r.w / 2 - 2) * std::abs(v));
            canvas.fill({v < 0 ? centre - length : centre, r.y + 2, length, r.h - 4}, selected);
        } else {
            canvas.fill({r.x + 2, r.y + 2, int((r.w - 4) * v), r.h - 4}, selected);
        }
        canvas.text(r.x + 5, r.y + (r.h - Canvas::text_height(m.text_scale)) / 2, m.label,
                    m.text_scale, text);
    }
    for (const auto &t : view.text)
        draw_text(t);
    for (const auto &d : view.toolbar)
        draw_key(d);
    if (!view.picker.empty()) {
        canvas.fill(rect(view.picker), panel);
        canvas.frame(rect(view.picker), 1, border);
    }
    for (const auto &d : view.picker_rows)
        draw_key(d);
    for (const auto &t : view.popup_text)
        draw_text(t);
}

struct Presenter {
    gpu::Device *device = nullptr;
    gpu::Swapchain chain;
    gpu::Texture texture;
    gpu::Format format = gpu::Format::BGRA8;
    int w = 0, h = 0, tex_w = 0, tex_h = 0;

    bool open(void *surface, int width, int height) {
        if (!device) {
            w = width;
            h = height;
            return SDL_GetWindowSurface(window) != nullptr;
        }
        chain = device->create_swapchain(surface, width, height);
        if (!chain)
            return false;
        format = device->swapchain_format(chain);
        w = width;
        h = height;
        return true;
    }
    void resize(int width, int height) {
        if (width == w && height == h)
            return;
        if (device)
            device->resize(chain, width, height);
        w = width;
        h = height;
    }
    SDL_Window *window = nullptr; // software presentation when there is no device
    void show(const Canvas &c) {
        if (!device) {
            SDL_Surface *dst = SDL_GetWindowSurface(window);
            if (!dst)
                return;
            SDL_Surface *src = SDL_CreateSurfaceFrom(
                c.width(), c.height(),
                format == gpu::Format::BGRA8 ? SDL_PIXELFORMAT_BGRA32 : SDL_PIXELFORMAT_RGBA32,
                static_cast<void *>(const_cast<uint8_t *>(c.pixels())), c.width() * 4);
            if (src) {
                SDL_BlitSurface(src, nullptr, dst, nullptr);
                SDL_DestroySurface(src);
            }
            SDL_UpdateWindowSurface(window);
            return;
        }
        if (!chain || c.width() <= 0 || c.height() <= 0)
            return;
        if (!texture || tex_w != c.width() || tex_h != c.height()) {
            if (texture)
                device->destroy(texture);
            texture = device->create_texture(
                {c.width(), c.height(), format, gpu::UsageSampled | gpu::UsageCpu, 1});
            tex_w = c.width();
            tex_h = c.height();
        }
        if (!texture)
            return;
        device->upload(texture, {0, 0, c.width(), c.height()}, c.pixels(), c.width() * 4);
        gpu::Texture drawable = device->acquire(chain);
        if (!drawable)
            return;
        gpu::TextureDesc d = device->describe(drawable);
        const int cw = std::min(d.width, c.width()), ch = std::min(d.height, c.height());
        gpu::CommandBuffer cb = device->begin();
        device->blit(cb, texture, {0, 0, cw, ch}, drawable, 0, 0);
        device->present(cb, chain, drawable, 0, nullptr);
        device->commit(cb);
        device->wait(cb);
    }
    void close() {
        if (!device) {
            SDL_DestroyWindowSurface(window);
            return;
        }
        if (texture)
            device->destroy(texture);
        texture = {};
        if (chain)
            device->destroy(chain);
        chain = {};
    }
};

ConfigureActions preferences_actions() {
    ConfigureActions actions;
    actions.read = [](std::vector<ConfigureSetting> *out, std::string *error) {
        if (!host_preferences::load(error))
            return false;
        std::size_t count = 0;
        const host_preferences::SettingInfo *infos = host_preferences::settings(&count);
        out->clear();
        for (std::size_t i = 0; i < count; ++i) {
            const host_preferences::SettingInfo &info = infos[i];
            ConfigureSetting setting;
            setting.id = info.id;
            setting.label = info.label;
            setting.description = info.description;
            setting.wrap = info.choices != nullptr;
            if (info.choices) {
                std::istringstream choices(info.choices);
                std::string value;
                while (std::getline(choices, value, '|'))
                    setting.values.push_back(value);
            } else {
                for (int value = info.minimum; value <= info.maximum; value += info.step)
                    setting.values.push_back(std::to_string(value) + "%");
            }
            const int raw = host_preferences::get(info.setting);
            setting.value = (raw - info.minimum) / info.step;
            setting.recommended = (info.recommended - info.minimum) / info.step;
            if (setting.value < 0 || setting.value >= int(setting.values.size()))
                setting.value =
                    std::clamp(setting.recommended, 0, std::max(0, int(setting.values.size()) - 1));
            out->push_back(std::move(setting));
        }
        if (error)
            error->clear();
        return true;
    };
    actions.set = [](const std::string &id, int index, std::string *error) {
        std::size_t count = 0;
        const host_preferences::SettingInfo *infos = host_preferences::settings(&count);
        for (std::size_t i = 0; i < count; ++i)
            if (id == infos[i].id) {
                const bool changed = host_preferences::set(
                    infos[i].setting, infos[i].minimum + index * infos[i].step, error);
                if (changed && infos[i].setting == host_preferences::Setting::KeyboardNames)
                    host_key_names_refresh();
                return changed;
            }
        if (error)
            *error = "This setting is unavailable.";
        return false;
    };
    actions.restore_recommended = [](std::string *error) { return host_preferences::reset(error); };
    return actions;
}

bool run_configure_ui(SDL_Window *window, gpu::Device *device, void *native_surface,
                      const std::string &game) {
    ConfigureUi ui(game, preferences_actions());
    SDL_InitSubSystem(SDL_INIT_GAMEPAD);
    SDL_ShowWindow(window);
    SDL_RaiseWindow(window);
    int dw = 0, dh = 0, bw = 0, bh = 0;
    SDL_GetWindowSizeInPixels(window, &dw, &dh);
    SDL_GetWindowSize(window, &bw, &bh);
    Presenter presenter;
    presenter.device = device;
    presenter.window = window;
    if (!presenter.open(native_surface, std::max(1, dw), std::max(1, dh))) {
        const std::string error =
            "Could not open the Configure display: " + std::string(SDL_GetError());
        fprintf(stderr, "[configure] %s\n", error.c_str());
        SDL_ShowSimpleMessageBox(SDL_MESSAGEBOX_ERROR, "Configure", error.c_str(), window);
        presenter.close();
        return false;
    }
    Canvas canvas;
    controls::PadMapperEditor controller;
    controls::host_set_focused(false);
    controls::host_mapper_load();
    auto screen = [&] {
        SDL_GetWindowSizeInPixels(window, &dw, &dh);
        SDL_GetWindowSize(window, &bw, &bh);
        return controls::Screen{dw, dh, bw > 0 ? double(dw) / bw : 1.0, {0, 0, dw, dh}, {}};
    };
    bool redraw = true;
    auto to_canvas = [&](float x, float y, int *cx, int *cy) {
        *cx = bw > 0 ? int(x * float(dw) / float(bw)) : int(x);
        *cy = bh > 0 ? int(y * float(dh) / float(bh)) : int(y);
    };
    while (!ui.done()) {
        SDL_Event event;
        if (SDL_WaitEventTimeout(&event, controller.is_open() ? 16 : 250)) {
            do {
                controls::gamepad_handle_event(event);
                controller.set_device(controller_description(), controls::gamepad_connected());
                controller.observe_input(controls::host_mapper_raw_state());
                if (controller.is_open()) {
                    controller.set_screen(screen());
                    switch (event.type) {
                    case SDL_EVENT_MOUSE_MOTION: {
                        int x, y;
                        to_canvas(event.motion.x, event.motion.y, &x, &y);
                        controller.pointer(x, y, 0);
                        break;
                    }
                    case SDL_EVENT_MOUSE_BUTTON_DOWN:
                    case SDL_EVENT_MOUSE_BUTTON_UP:
                        if (event.button.button == SDL_BUTTON_LEFT) {
                            int x, y;
                            to_canvas(event.button.x, event.button.y, &x, &y);
                            controller.pointer(x, y,
                                               event.type == SDL_EVENT_MOUSE_BUTTON_DOWN ? 1 : -1);
                        }
                        break;
                    case SDL_EVENT_MOUSE_WHEEL:
                        controller.wheel(event.wheel.direction == SDL_MOUSEWHEEL_FLIPPED
                                             ? -event.wheel.y
                                             : event.wheel.y);
                        break;
                    case SDL_EVENT_FINGER_DOWN:
                    case SDL_EVENT_FINGER_MOTION:
                    case SDL_EVENT_FINGER_UP:
                        controller.pointer(event.tfinger.x * dw, event.tfinger.y * dh,
                                           event.type == SDL_EVENT_FINGER_DOWN ? 1
                                           : event.type == SDL_EVENT_FINGER_UP ? -1
                                                                               : 0);
                        break;
                    case SDL_EVENT_KEY_DOWN:
                    case SDL_EVENT_KEY_UP:
                        controller.key(event.key.scancode, event.type == SDL_EVENT_KEY_DOWN);
                        break;
                    case SDL_EVENT_WINDOW_FOCUS_LOST:
                        controller.suspend();
                        break;
                    default:
                        break;
                    }
                    if (event.type == SDL_EVENT_QUIT)
                        controller.request_close();
                    continue;
                }
                switch (event.type) {
                case SDL_EVENT_QUIT:
                    ui.key(Key::Back);
                    break;
                case SDL_EVENT_WINDOW_PIXEL_SIZE_CHANGED:
                case SDL_EVENT_WINDOW_RESIZED:
                case SDL_EVENT_WINDOW_EXPOSED:
                    redraw = true;
                    break;
                case SDL_EVENT_KEYMAP_CHANGED:
                    host_key_names_refresh();
                    break;
                case SDL_EVENT_MOUSE_BUTTON_DOWN:
                case SDL_EVENT_MOUSE_BUTTON_UP:
                    if (event.button.button == SDL_BUTTON_LEFT) {
                        int x, y;
                        to_canvas(event.button.x, event.button.y, &x, &y);
                        ui.pointer(x, y, event.type == SDL_EVENT_MOUSE_BUTTON_DOWN);
                    }
                    break;
                case SDL_EVENT_FINGER_DOWN:
                case SDL_EVENT_FINGER_UP:
                    ui.pointer(int(event.tfinger.x * float(dw)), int(event.tfinger.y * float(dh)),
                               event.type == SDL_EVENT_FINGER_DOWN);
                    break;
                case SDL_EVENT_KEY_DOWN:
                    switch (event.key.scancode) {
                    case SDL_SCANCODE_UP:
                        ui.key(Key::Up);
                        break;
                    case SDL_SCANCODE_DOWN:
                        ui.key(Key::Down);
                        break;
                    case SDL_SCANCODE_LEFT:
                        ui.key(Key::Left);
                        break;
                    case SDL_SCANCODE_RIGHT:
                        ui.key(Key::Right);
                        break;
                    case SDL_SCANCODE_TAB:
                        ui.key((event.key.mod & SDL_KMOD_SHIFT) ? Key::Previous : Key::Next);
                        break;
                    case SDL_SCANCODE_RETURN:
                    case SDL_SCANCODE_KP_ENTER:
                    case SDL_SCANCODE_SPACE:
                        ui.key(Key::Activate);
                        break;
                    case SDL_SCANCODE_ESCAPE:
                    case SDL_SCANCODE_AC_BACK:
                        ui.key(Key::Back);
                        break;
                    default:
                        break;
                    }
                    break;
                case SDL_EVENT_GAMEPAD_BUTTON_DOWN:
                    switch (event.gbutton.button) {
                    case SDL_GAMEPAD_BUTTON_DPAD_UP:
                        ui.key(Key::Up);
                        break;
                    case SDL_GAMEPAD_BUTTON_DPAD_DOWN:
                        ui.key(Key::Down);
                        break;
                    case SDL_GAMEPAD_BUTTON_DPAD_LEFT:
                        ui.key(Key::Left);
                        break;
                    case SDL_GAMEPAD_BUTTON_DPAD_RIGHT:
                        ui.key(Key::Right);
                        break;
                    case SDL_GAMEPAD_BUTTON_SOUTH:
                    case SDL_GAMEPAD_BUTTON_START:
                        ui.key(Key::Activate);
                        break;
                    case SDL_GAMEPAD_BUTTON_EAST:
                    case SDL_GAMEPAD_BUTTON_BACK:
                        ui.key(Key::Back);
                        break;
                    default:
                        break;
                    }
                    break;
                default:
                    break;
                }
                if (ui.take_controller_request()) {
                    controls::host_mapper_load();
                    controller.open(controls::host_mapper_profile(), screen());
                    redraw = true;
                }
            } while (SDL_PollEvent(&event));
        }
        controls::gamepad_poll();
        controller.set_device(controller_description(), controls::gamepad_connected());
        const controls::Screen controller_screen = screen();
        if (controller.is_open()) {
            const auto action = controller.take_action();
            if (action == controls::PadMapperEditorAction::TestRumble) {
                if (!controls::gamepad_connected())
                    controller.set_message("No controller connected.");
                else if (!controls::gamepad_rumble_supported())
                    controller.set_message("This controller reports no rumble support to SDL.");
                else if (!controls::gamepad_rumble(0xffff, 0xffff, 1000))
                    controller.set_message("Rumble failed: " + std::string(SDL_GetError()));
                else
                    controller.set_message("Rumble sent for one second. Did you feel it?");
            }
            if (action == controls::PadMapperEditorAction::CloseWindow)
                ui.key(Key::Back);
            if (action == controls::PadMapperEditorAction::CloseWindow ||
                action == controls::PadMapperEditorAction::Cancel ||
                (action == controls::PadMapperEditorAction::Apply &&
                 controls::host_mapper_apply(controller.profile(), true))) {
                controller.close();
                redraw = true;
            }
        }
        if (dw <= 0 || dh <= 0)
            continue;
        if (redraw || controller.is_open() || ui.dirty() || canvas.width() != dw ||
            canvas.height() != dh) {
            redraw = false;
            presenter.resize(dw, dh);
            const bool bgra = presenter.format == gpu::Format::BGRA8;
            canvas.resize(dw, dh, bgra);
            const int scale = std::max(1, std::min(dw / 640, dh / 520));
            if (controller.is_open()) {
                controls::PadMapper preview;
                controls::PadState calibrated, guest;
                const controls::PadState raw = controls::host_mapper_raw_state();
                std::string error;
                std::string status = controls::host_mapper_status();
                if (preview.set_profile(controller.profile(), &error))
                    preview.process(raw, &calibrated, &guest);
                else
                    status = "Invalid controller profile draft: " + error;
                controller.set_screen(controller_screen);
                const auto view =
                    controller.view(controller_screen, raw, calibrated, guest, status);
                draw_controller_page(canvas, view);
            } else {
                ui.draw(canvas, scale);
            }
            presenter.show(canvas);
        }
    }
    controller.close();
    controls::host_gamepad_state({});
    controls::host_set_focused(true);
    presenter.close();
    return ui.ready();
}

void write_ppm(const std::string &path, const Canvas &c, bool bgra) {
    FILE *f = fopen((path + ".part").c_str(), "wb");
    if (!f)
        return;
    fprintf(f, "P6\n%d %d\n255\n", c.width(), c.height());
    std::vector<uint8_t> row(size_t(c.width()) * 3);
    for (int y = 0; y < c.height(); ++y) {
        const uint8_t *src = c.pixels() + size_t(y) * size_t(c.width()) * 4;
        for (int x = 0; x < c.width(); ++x) {
            row[size_t(x) * 3] = bgra ? src[x * 4 + 2] : src[x * 4];
            row[size_t(x) * 3 + 1] = src[x * 4 + 1];
            row[size_t(x) * 3 + 2] = bgra ? src[x * 4] : src[x * 4 + 2];
        }
        fwrite(row.data(), 1, row.size(), f);
    }
    fclose(f);
    os_rename((path + ".part").c_str(), path.c_str());
}

} // namespace

bool configure(SDL_Window *window, gpu::Device *device, void *native_surface) {
    if (!window)
        return false;
    std::string error;
    if (!host_preferences::reload(&error)) {
        fprintf(stderr, "[configure] %s\n", error.c_str());
        SDL_ShowSimpleMessageBox(SDL_MESSAGEBOX_ERROR, "Configure", error.c_str(), window);
        return false;
    }
    const char *window_title = SDL_GetWindowTitle(window);
    const std::string previous_title = window_title ? window_title : "";
    int previous_width = 0, previous_height = 0;
    SDL_GetWindowSize(window, &previous_width, &previous_height);
    int minimum_width = 0, minimum_height = 0;
    SDL_GetWindowMinimumSize(window, &minimum_width, &minimum_height);
    SDL_SetWindowMinimumSize(window, std::max(minimum_width, 640), std::max(minimum_height, 520));
    const std::string configure_title = "Configure " + spec_from_config().title;
    SDL_SetWindowTitle(window, configure_title.c_str());
    const bool resized = previous_width < 640 || previous_height < 520;
    if (resized)
        SDL_SetWindowSize(window, std::max(previous_width, 640), std::max(previous_height, 520));
    const bool result = run_configure_ui(window, device, native_surface, spec_from_config().title);
    SDL_SetWindowMinimumSize(window, minimum_width, minimum_height);
    if (resized)
        SDL_SetWindowSize(window, previous_width, previous_height);
    SDL_SetWindowTitle(window, previous_title.c_str());
    return result;
}

std::string run(SDL_Window *window, gpu::Device *device, void *native_surface, Platform &platform,
                const RunOptions &options) {
    const Spec &spec = spec_from_config();
    {
        const PlatformInfo info = platform.info();
        fprintf(stderr, "[launcher] game data %s, saves %s\n", info.import_root.c_str(),
                info.profile_dir.c_str());
    }
    Launcher launcher(spec, platform);
    launcher.start(options.known);
    if (!options.unplayable.empty())
        launcher.set_unplayable(options.unplayable);
    if (options.auto_play > 0)
        launcher.set_auto_play(options.auto_play);

    SDL_InitSubSystem(SDL_INIT_GAMEPAD);
    SDL_ShowWindow(window);
    SDL_RaiseWindow(window);
    int dw = 0, dh = 0, bw = 0, bh = 0;
    SDL_GetWindowSizeInPixels(window, &dw, &dh);
    SDL_GetWindowSize(window, &bw, &bh);
    Presenter presenter;
    presenter.device = device;
    presenter.window = window;
    if (!presenter.open(native_surface, std::max(1, dw), std::max(1, dh))) {
        fprintf(stderr, "[launcher] no swapchain for the window: %s\n", SDL_GetError());
        return "";
    }
    Canvas canvas;
    std::vector<SDL_Gamepad *> pads;
    uint64_t last = SDL_GetTicksNS();
    bool redraw = true;
    auto to_canvas = [&](float x, float y, int *cx, int *cy) {
        *cx = bw > 0 ? int(x * float(dw) / float(bw)) : int(x);
        *cy = bh > 0 ? int(y * float(dh) / float(bh)) : int(y);
    };
    std::vector<std::string> script;
    for (size_t start = 0; start < options.keys.size();) {
        size_t end = options.keys.find(',', start);
        if (end == std::string::npos)
            end = options.keys.size();
        script.push_back(options.keys.substr(start, end - start));
        start = end + 1;
    }
    size_t script_step = 0;
    uint64_t next_step = SDL_GetTicksNS() + 500000000ull;
    while (!launcher.finished() && !launcher.quit()) {
        if (script_step < script.size() && SDL_GetTicksNS() >= next_step &&
            launcher.screen() != Screen::Importing) {
            const std::string &k = script[script_step++];
            next_step = SDL_GetTicksNS() + 300000000ull;
            fprintf(stderr, "[launcher] script: %s\n", k.c_str());
            if (k == "up")
                launcher.key(Key::Up);
            else if (k == "down")
                launcher.key(Key::Down);
            else if (k == "left")
                launcher.key(Key::Left);
            else if (k == "right")
                launcher.key(Key::Right);
            else if (k == "next")
                launcher.key(Key::Next);
            else if (k == "previous")
                launcher.key(Key::Previous);
            else if (k == "enter")
                launcher.key(Key::Activate);
            else if (k == "back")
                launcher.key(Key::Back);
            else if (k.compare(0, 5, "drop:") == 0)
                launcher.drop(k.substr(5));
        }
        SDL_Event e;
        const bool busy = launcher.screen() == Screen::Importing || launcher.counting_down();
        if (SDL_WaitEventTimeout(&e, busy ? 50 : 250)) {
            do {
                switch (e.type) {
                case SDL_EVENT_QUIT:
                    for (SDL_Gamepad *g : pads)
                        SDL_CloseGamepad(g);
                    presenter.close();
                    return "";
                case SDL_EVENT_WINDOW_PIXEL_SIZE_CHANGED:
                case SDL_EVENT_WINDOW_RESIZED:
                case SDL_EVENT_WINDOW_EXPOSED:
                    redraw = true;
                    break;
                case SDL_EVENT_KEYMAP_CHANGED:
                    host_key_names_refresh();
                    break;
                case SDL_EVENT_MOUSE_BUTTON_DOWN:
                case SDL_EVENT_MOUSE_BUTTON_UP:
                    if (e.button.button == SDL_BUTTON_LEFT) {
                        int x, y;
                        to_canvas(e.button.x, e.button.y, &x, &y);
                        launcher.pointer(x, y, e.type == SDL_EVENT_MOUSE_BUTTON_DOWN);
                    }
                    break;
                case SDL_EVENT_FINGER_DOWN:
                case SDL_EVENT_FINGER_UP:
                    launcher.pointer(int(e.tfinger.x * float(dw)), int(e.tfinger.y * float(dh)),
                                     e.type == SDL_EVENT_FINGER_DOWN);
                    break;
                case SDL_EVENT_KEY_DOWN:
                    switch (e.key.scancode) {
                    case SDL_SCANCODE_UP:
                        launcher.key(Key::Up);
                        break;
                    case SDL_SCANCODE_DOWN:
                        launcher.key(Key::Down);
                        break;
                    case SDL_SCANCODE_LEFT:
                        launcher.key(Key::Left);
                        break;
                    case SDL_SCANCODE_RIGHT:
                        launcher.key(Key::Right);
                        break;
                    case SDL_SCANCODE_TAB:
                        launcher.key((e.key.mod & SDL_KMOD_SHIFT) ? Key::Previous : Key::Next);
                        break;
                    case SDL_SCANCODE_RETURN:
                    case SDL_SCANCODE_KP_ENTER:
                    case SDL_SCANCODE_SPACE:
                        launcher.key(Key::Activate);
                        break;
                    case SDL_SCANCODE_ESCAPE:
                    case SDL_SCANCODE_AC_BACK:
                        launcher.key(Key::Back);
                        break;
                    default:
                        // Any key stops a countdown and does nothing else.
                        if (launcher.counting_down())
                            launcher.key(Key::Back);
                        break;
                    }
                    break;
                case SDL_EVENT_GAMEPAD_ADDED:
                    if (SDL_Gamepad *g = SDL_OpenGamepad(e.gdevice.which))
                        pads.push_back(g);
                    break;
                case SDL_EVENT_GAMEPAD_BUTTON_DOWN:
                    switch (e.gbutton.button) {
                    case SDL_GAMEPAD_BUTTON_DPAD_UP:
                        launcher.key(Key::Up);
                        break;
                    case SDL_GAMEPAD_BUTTON_DPAD_DOWN:
                        launcher.key(Key::Down);
                        break;
                    case SDL_GAMEPAD_BUTTON_DPAD_LEFT:
                        launcher.key(Key::Left);
                        break;
                    case SDL_GAMEPAD_BUTTON_DPAD_RIGHT:
                        launcher.key(Key::Right);
                        break;
                    case SDL_GAMEPAD_BUTTON_SOUTH:
                    case SDL_GAMEPAD_BUTTON_START:
                        launcher.key(Key::Activate);
                        break;
                    case SDL_GAMEPAD_BUTTON_EAST:
                    case SDL_GAMEPAD_BUTTON_BACK:
                        launcher.key(Key::Back);
                        break;
                    default:
                        break;
                    }
                    break;
                case SDL_EVENT_DROP_FILE:
                    if (e.drop.data)
                        launcher.drop(e.drop.data);
                    break;
                default:
                    break;
                }
            } while (SDL_PollEvent(&e));
        }
        // The countdown runs only while it can be seen, in small steps, so a
        // slow first frame (an app still launching) does not eat it.
        const uint64_t now = SDL_GetTicksNS();
        if (canvas.width() > 0)
            launcher.advance(std::min(0.1, double(now - last) / 1e9));
        last = now;
        launcher.tick();
        if (launcher.configure_requested()) {
            launcher.clear_configure_request();
            presenter.close();
            if (!configure(window, device, native_surface))
                launcher.show_message("Configure could not load preferences or open the window.");
            SDL_GetWindowSizeInPixels(window, &dw, &dh);
            if (!presenter.open(native_surface, std::max(1, dw), std::max(1, dh))) {
                fprintf(stderr, "[launcher] no swapchain for the window: %s\n", SDL_GetError());
                for (SDL_Gamepad *g : pads)
                    SDL_CloseGamepad(g);
                return "";
            }
            redraw = true;
        }
        {
            static int last_state = -1, last_screen = -1;
            const int st = int(launcher.status().state), sc = int(launcher.screen());
            if (st != last_state || sc != last_screen) {
                fprintf(stderr, "[launcher] %s, screen %d%s%s\n",
                        state_name(launcher.status().state), sc,
                        launcher.screen() == Screen::Message ? ": " : "",
                        launcher.screen() == Screen::Message ? launcher.message().c_str() : "");
                last_state = st;
                last_screen = sc;
            }
        }
        if (launcher.finished() || launcher.quit())
            break; // the last frame shown stays what the player saw
        SDL_GetWindowSizeInPixels(window, &dw, &dh);
        SDL_GetWindowSize(window, &bw, &bh);
        if (dw <= 0 || dh <= 0)
            continue;
        if (redraw || launcher.dirty() || canvas.width() != dw || canvas.height() != dh) {
            redraw = false;
            presenter.resize(dw, dh);
            const bool bgra = presenter.format == gpu::Format::BGRA8;
            canvas.resize(dw, dh, bgra);
            const int scale = std::max(1, std::min(dw / 480, dh / 300));
            launcher.draw(canvas, scale);
            presenter.show(canvas);
            if (!options.dump_path.empty())
                write_ppm(options.dump_path, canvas, bgra);
        }
    }
    for (SDL_Gamepad *g : pads)
        SDL_CloseGamepad(g);
    presenter.close();
    if (launcher.quit())
        return "";
    return launcher.exe();
}

} // namespace launcher
