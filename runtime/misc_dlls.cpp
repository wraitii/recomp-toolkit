// Small DLL probes for a runtime with no printer, workstation service or tray.
#include "imports.h"
#include "user32_internal.h"
#include "win32.h"
#include "../platform/os.h"
#include <algorithm>
#include <cstring>
#include <vector>

namespace {
uint32_t msvcr71_app_type;
void crt_set_app_type(X86 *c) {
    msvcr71_app_type = arg(c, 0);
    set_eax(c, msvcr71_app_type);
}
void zero_out(uint32_t p) {
    if (p && gm_valid(p, 4))
        wr32(p, 0);
}
void yes(X86 *c) {
    set_eax(c, 1);
}
void zero(X86 *c) {
    set_eax(c, 0);
}

// HAP Debugger: a developer-only remote debug console that owns its own
// sockets and Python interpreter. Its imports are __cdecl (mangled names with
// no @N), so the dispatcher must leave the arguments for the caller to pop.
// Initialize reports "no debugger" so the title's HAP session path returns
// early, and the output calls are accepted and dropped, as on a run with no
// debugger attached.
void hapdbg_noop(X86 *c) {
    set_eax(c, 0);
}

// SHLWAPI string and path helpers msxml3 imports. The URL entry points that
// need WinINet's parser stay named failures.
uint16_t wide_unit(uint32_t p, uint32_t i) {
    return p && gm_valid(p + i * 2, 2) ? rd16(p + i * 2) : 0;
}
uint16_t lower_unit(uint16_t ch) {
    return ch >= 'A' && ch <= 'Z' ? (uint16_t)(ch + 32) : ch;
}
void s_StrCmpW(X86 *c) {
    uint32_t a = arg(c, 0), b = arg(c, 1);
    for (uint32_t i = 0;; ++i) {
        uint16_t ca = wide_unit(a, i), cb = wide_unit(b, i);
        if (ca != cb) {
            set_eax(c, ca < cb ? 0xffffffffu : 1u);
            return;
        }
        if (!ca) {
            set_eax(c, 0);
            return;
        }
    }
}
void s_StrCmpNIW(X86 *c) {
    uint32_t a = arg(c, 0), b = arg(c, 1), n = arg(c, 2);
    for (uint32_t i = 0; i < n; ++i) {
        uint16_t ca = lower_unit(wide_unit(a, i)), cb = lower_unit(wide_unit(b, i));
        if (ca != cb) {
            set_eax(c, ca < cb ? 0xffffffffu : 1u);
            return;
        }
        if (!ca)
            break;
    }
    set_eax(c, 0);
}
void s_StrToIntW(X86 *c) {
    uint32_t p = arg(c, 0), i = 0;
    uint16_t ch = wide_unit(p, i);
    while (ch == ' ' || ch == '\t')
        ch = wide_unit(p, ++i);
    bool minus = ch == '-';
    if (minus || ch == '+')
        ch = wide_unit(p, ++i);
    int32_t value = 0;
    while (ch >= '0' && ch <= '9') {
        value = value * 10 + (int32_t)(ch - '0');
        ch = wide_unit(p, ++i);
    }
    set_eax(c, (uint32_t)(minus ? -value : value));
}
void s_PathFindExtensionW(X86 *c) {
    uint32_t p = arg(c, 0), len = 0;
    while (wide_unit(p, len))
        ++len;
    for (uint32_t i = len; i-- > 0;) {
        uint16_t ch = wide_unit(p, i);
        if (ch == '.') {
            set_eax(c, p + i * 2);
            return;
        }
        if (ch == '\\' || ch == '/' || ch == ':')
            break;
    }
    set_eax(c, p + len * 2);
}
void s_PathFindFileNameW(X86 *c) {
    uint32_t p = arg(c, 0), name = p;
    for (uint32_t i = 0; wide_unit(p, i); ++i) {
        uint16_t ch = wide_unit(p, i);
        if (ch == '\\' || ch == '/')
            name = p + (i + 1) * 2;
    }
    set_eax(c, name);
}
void s_PathIsURLW(X86 *c) {
    uint32_t p = arg(c, 0), i = 0;
    uint16_t ch = wide_unit(p, 0);
    if (!((ch >= 'A' && ch <= 'Z') || (ch >= 'a' && ch <= 'z'))) {
        set_eax(c, 0);
        return;
    }
    for (i = 1;; ++i) {
        ch = wide_unit(p, i);
        if ((ch >= 'A' && ch <= 'Z') || (ch >= 'a' && ch <= 'z') || (ch >= '0' && ch <= '9') ||
            ch == '+' || ch == '-' || ch == '.')
            continue;
        break;
    }
    set_eax(c, i >= 2 && ch == ':' ? 1 : 0);
}
// DSETUP.dll is the shipped DirectSetup shim; runblack.exe imports only
// DirectXSetupGetVersion by ordinal and refuses to start below 0x00040007
// ("You need DirectX 7 or greater"). The DLL's own file version is 4.0.7.700,
// so that is what its fallback path reports when the registry has no value.
void directx_setup_get_version(X86 *c) {
    if (uint32_t p = arg(c, 0); p && gm_valid(p, 4))
        wr32(p, 0x00040007);
    if (uint32_t p = arg(c, 1); p && gm_valid(p, 4))
        wr32(p, 700);
    set_eax(c, 1); // S_OK, as the shipped DirectSetup returns
}
void session_service_unavailable(X86 *c) {
    // No Remote Desktop Services endpoint is running in the guest runtime.
    // Expose the exports so callers receive the service error as a BOOL result.
    set_last_error(1702); // RPC_S_INVALID_BINDING
    set_eax(c, 0);
}
void buffered_paint_unavailable(X86 *c) {
    set_eax(c, 0x80004001u); // E_NOTIMPL: use the caller's ordinary GDI path.
}
// Visual styles are on, as on every Windows since Vista, but there is no theme
// data: OpenThemeData finds none, so a program draws its controls the classic
// way, and the per-part queries fail with E_HANDLE as they do for a NULL theme.
// A VCL program binds every export at start and calls through the pointers,
// so each one is here. DrawThemeParentBackground is the one that does work.
void e_handle(X86 *c) {
    set_eax(c, 0x80070006u); // E_HANDLE
}
void theme_app_properties(X86 *c) {
    set_eax(c, 7); // STAP_ALLOW_NONCLIENT | STAP_ALLOW_CONTROLS | STAP_ALLOW_WEBCONTENT
}
// With no theme, the system metric queries answer from the system settings.
void theme_sys_value(X86 *c, const char *user32_export) {
    set_eax(c, guest_call(c, imports_resolve("USER32.dll", user32_export), arg(c, 1)));
}
void theme_sys_color(X86 *c) {
    theme_sys_value(c, "GetSysColor");
}
void theme_sys_color_brush(X86 *c) {
    theme_sys_value(c, "GetSysColorBrush");
}
void theme_sys_size(X86 *c) {
    theme_sys_value(c, "GetSystemMetrics");
}
// With no theme data, themed text is drawn the classic way: the DC's font, the
// colour DTTOPTS names if it names one, and no background, as themed text never
// has one. Windows answers a NULL theme with E_HANDLE, but a VCL program with
// styles on draws its captions through these calls, and would lose them all.
uint32_t theme_text(uint32_t hdc, uint32_t text, uint32_t count, uint32_t flags, uint32_t rect,
                    uint32_t options) {
    auto *dc = gdi::dc_of(hdc);
    if (!dc || !text || !rect)
        return 0x80070057u; // E_INVALIDARG
    const uint32_t option_flags = options && gm_valid(options, 12) ? rd32(options + 4) : 0;
    const uint32_t old_color = dc->text_color;
    const int old_mode = dc->bk_mode;
    if (option_flags & 0x1) // DTT_TEXTCOLOR
        dc->text_color = rd32(options + 8);
    if (option_flags & 0x200) // DTT_CALCRECT
        flags |= 0x400;       // DT_CALCRECT
    dc->bk_mode = 1;          // TRANSPARENT
    gdi::draw_text(hdc, text, count, rect, flags);
    if (auto *after = gdi::dc_of(hdc)) {
        after->text_color = old_color;
        after->bk_mode = old_mode;
    }
    return 0;
}
// DrawThemeText(hTheme, hdc, part, state, text, count, flags, flags2, rect)
void draw_theme_text(X86 *c) {
    set_eax(c, theme_text(arg(c, 1), arg(c, 4), arg(c, 5), arg(c, 6), arg(c, 8), 0));
}
// DrawThemeTextEx(hTheme, hdc, part, state, text, count, flags, rect, options)
void draw_theme_text_ex(X86 *c) {
    set_eax(c, theme_text(arg(c, 1), arg(c, 4), arg(c, 5), arg(c, 6), arg(c, 7), arg(c, 8)));
}
// No paint buffer and no animation: the caller paints its target directly.
void begin_buffered_paint(X86 *c) {
    if (recomp_env("TRACE_GDI")) {
        const uint32_t rc = arg(c, 1);
        LOGW("gdi: BeginBufferedPaint target=%08x rect=%d,%d,%d,%d format=%u", arg(c, 0),
             rc && gm_valid(rc, 16) ? (int)rd32(rc) : -1,
             rc && gm_valid(rc, 16) ? (int)rd32(rc + 4) : -1,
             rc && gm_valid(rc, 16) ? (int)rd32(rc + 8) : -1,
             rc && gm_valid(rc, 16) ? (int)rd32(rc + 12) : -1, arg(c, 2));
    }
    zero_out(arg(c, 4)); // *phdc
    set_eax(c, 0);
}
void begin_buffered_animation(X86 *c) {
    zero_out(arg(c, 6)); // *phdcFrom
    zero_out(arg(c, 7)); // *phdcTo
    set_eax(c, 0);
}
// DrawThemeParentBackground(hwnd, hdc, prc): the parent paints its background
// into the child's DC, the DC's origin moved so the parent's coordinates land
// on the child, through WM_ERASEBKGND and then WM_PRINTCLIENT. A transparent
// control - the VCL's TStaticText - draws its text over what that leaves.
void draw_parent_background(X86 *c) {
    uint32_t hwnd = arg(c, 0), hdc = arg(c, 1), rect = arg(c, 2);
    auto *w = user32::find_window(hwnd);
    auto *dc = gdi::dc_of(hdc);
    if (!w || !dc) {
        set_eax(c, 0x80070006u); // E_HANDLE
        return;
    }
    const uint32_t parent = w->parent;
    if (!parent) {
        set_eax(c, 0);
        return;
    }
    int32_t cx = 0, cy = 0, px = 0, py = 0;
    user32::client_origin(hwnd, &cx, &cy);
    user32::client_origin(parent, &px, &py);
    const gdi::DcState saved = *dc;
    if (rect && gm_valid(rect, 16)) { // prc limits the paint, in the child's coordinates
        gdi::Rect cut = gdi::to_device(hdc, {int32_t(rd32(rect)), int32_t(rd32(rect + 4)),
                                             int32_t(rd32(rect + 8)), int32_t(rd32(rect + 12))});
        std::vector<gdi::Rect> clip;
        for (gdi::Rect r : dc->clipped ? dc->clip : std::vector<gdi::Rect>{cut}) {
            gdi::Rect hit{std::max(r.l, cut.l), std::max(r.t, cut.t), std::min(r.r, cut.r),
                          std::min(r.b, cut.b)};
            if (hit.l < hit.r && hit.t < hit.b)
                clip.push_back(hit);
        }
        dc->clip = std::move(clip);
        dc->clipped = true;
    }
    dc->org_x += cx - px;
    dc->org_y += cy - py;
    host_dispatch_to_wndproc(c, parent, 0x0014 /* WM_ERASEBKGND */, hdc, 0);
    host_dispatch_to_wndproc(c, parent, 0x0318 /* WM_PRINTCLIENT */, hdc, 4 /* PRF_CLIENT */);
    if (auto *after = gdi::dc_of(hdc)) // the window procedures may have changed it
        static_cast<gdi::DcState &>(*after) = saved;
    set_eax(c, 0);
}
void composition_enabled(X86 *c) {
    uint32_t out = arg(c, 0);
    if (!out || !gm_valid(out, 4)) {
        set_eax(c, 0x80070057u); // E_INVALIDARG
        return;
    }
    wr32(out, 0); // No desktop composition or glass frame in the guest model.
    set_eax(c, 0);
}
void enum_printers(X86 *c) {
    zero_out(arg(c, 5));
    zero_out(arg(c, 6));
    set_eax(c, 1);
}
void default_printer(X86 *c) {
    zero_out(arg(c, 1));
    set_last_error(2);
    set_eax(c, 0);
}
void open_printer(X86 *c) {
    zero_out(arg(c, 1));
    set_last_error(1801); // ERROR_INVALID_PRINTER_NAME
    set_eax(c, 0);
}
void document_properties(X86 *c) {
    set_eax(c, 0xffffffffu);
}
// The Win32 prototype has three arguments, not four: server, level, buffer.
void workstation_info(X86 *c) {
    zero_out(arg(c, 2));
    set_eax(c, 50);
}
void crt_memcpy(X86 *c) {
    uint32_t dst = arg(c, 0), src = arg(c, 1), size = arg(c, 2);
    if (gm_valid(dst, size) && gm_valid(src, size) && size)
        memcpy(g_mem + dst, g_mem + src, size);
    set_eax(c, dst);
}
void crt_memset(X86 *c) {
    uint32_t dst = arg(c, 0), size = arg(c, 2);
    if (gm_valid(dst, size) && size)
        memset(g_mem + dst, int(arg(c, 1)), size);
    set_eax(c, dst);
}
// All supported per-user settings locations use the same Documents guest path;
// the file overlay decides where that path resides in the writable profile.
void url_download_to_file(X86 *c) {
    imports_decline(c, "URLDownloadToFileW", 0x800c0008u /* INET_E_DOWNLOAD_FAILURE */);
}
void folder_path(X86 *c) {
    uint32_t csidl = arg(c, 1), out = arg(c, 4);
    switch (csidl & 0xff) {
    case 5:
    case 26:
    case 28:
    case 35:
    case 0x02:
    case 0x0d:
    case 0x10:
    case 0x14:
    case 0x17:
    case 0x19:
    case 0x2e:
        break;
    default:
        set_eax(c, 0x80070057u);
        return;
    }
    if (!out || !gm_valid(out, 520)) {
        set_eax(c, 0x80070057u);
        return;
    }
    gm_put_wstr(out, shell_folder_guest_path(5, (csidl & 0x8000) != 0), 260);
    set_eax(c, 0);
}
const ImportShim shims[] = {
    // Ordinal 11 is DirectXSetupGetVersion; the name is registered too so a
    // GetProcAddress by name answers.
    {"DSETUP.dll", "ord11", 2, directx_setup_get_version},
    {"DSETUP.dll", "DirectXSetupGetVersion", 2, directx_setup_get_version},
    {"DWMAPI.dll", "DwmIsCompositionEnabled", 1, composition_enabled},
    {"DWMAPI.dll", "DwmExtendFrameIntoClientArea", 2, buffered_paint_unavailable},
    {"UXTHEME.dll", "IsThemeActive", 0, yes},
    {"UXTHEME.dll", "IsAppThemed", 0, yes},
    {"UXTHEME.dll", "OpenThemeData", 2, zero},
    {"UXTHEME.dll", "GetWindowTheme", 1, zero},
    {"UXTHEME.dll", "CloseThemeData", 1, zero},
    {"UXTHEME.dll", "SetWindowTheme", 3, zero},
    {"UXTHEME.dll", "EnableThemeDialogTexture", 2, zero},
    {"UXTHEME.dll", "IsThemeDialogTextureEnabled", 1, zero},
    {"UXTHEME.dll", "EnableTheming", 1, zero},
    {"UXTHEME.dll", "GetThemeAppProperties", 0, theme_app_properties},
    {"UXTHEME.dll", "SetThemeAppProperties", 1, zero},
    {"UXTHEME.dll", "IsThemePartDefined", 3, zero},
    {"UXTHEME.dll", "IsThemeBackgroundPartiallyTransparent", 3, zero},
    {"UXTHEME.dll", "GetThemeSysBool", 2, zero},
    {"UXTHEME.dll", "GetThemeSysColor", 2, theme_sys_color},
    {"UXTHEME.dll", "GetThemeSysColorBrush", 2, theme_sys_color_brush},
    {"UXTHEME.dll", "GetThemeSysSize", 2, theme_sys_size},
    {"UXTHEME.dll", "GetThemeSysFont", 3, e_handle},
    {"UXTHEME.dll", "GetThemeSysString", 4, e_handle},
    {"UXTHEME.dll", "GetThemeSysInt", 3, e_handle},
    {"UXTHEME.dll", "DrawThemeBackground", 6, e_handle},
    {"UXTHEME.dll", "DrawThemeText", 9, draw_theme_text},
    {"UXTHEME.dll", "DrawThemeTextEx", 9, draw_theme_text_ex},
    {"UXTHEME.dll", "OpenThemeDataForDpi", 3, zero},
    {"UXTHEME.dll", "BeginBufferedPaint", 5, begin_buffered_paint},
    {"UXTHEME.dll", "EndBufferedPaint", 2, buffered_paint_unavailable},
    {"UXTHEME.dll", "BufferedPaintSetAlpha", 3, buffered_paint_unavailable},
    {"UXTHEME.dll", "BeginBufferedAnimation", 8, begin_buffered_animation},
    {"UXTHEME.dll", "EndBufferedAnimation", 2, buffered_paint_unavailable},
    {"UXTHEME.dll", "BufferedPaintRenderAnimation", 2, zero},
    {"UXTHEME.dll", "BufferedPaintStopAllAnimations", 1, zero},
    {"UXTHEME.dll", "GetThemeBackgroundContentRect", 6, e_handle},
    {"UXTHEME.dll", "GetThemeBackgroundExtent", 6, e_handle},
    {"UXTHEME.dll", "GetThemePartSize", 7, e_handle},
    {"UXTHEME.dll", "GetThemeTextExtent", 9, e_handle},
    {"UXTHEME.dll", "GetThemeTextMetrics", 5, e_handle},
    {"UXTHEME.dll", "GetThemeBackgroundRegion", 6, e_handle},
    {"UXTHEME.dll", "HitTestThemeBackground", 10, e_handle},
    {"UXTHEME.dll", "DrawThemeEdge", 8, e_handle},
    {"UXTHEME.dll", "DrawThemeIcon", 7, e_handle},
    {"UXTHEME.dll", "GetThemeColor", 5, e_handle},
    {"UXTHEME.dll", "GetThemeMetric", 6, e_handle},
    {"UXTHEME.dll", "GetThemeString", 6, e_handle},
    {"UXTHEME.dll", "GetThemeBool", 5, e_handle},
    {"UXTHEME.dll", "GetThemeInt", 5, e_handle},
    {"UXTHEME.dll", "GetThemeEnumValue", 5, e_handle},
    {"UXTHEME.dll", "GetThemePosition", 5, e_handle},
    {"UXTHEME.dll", "GetThemeFont", 6, e_handle},
    {"UXTHEME.dll", "GetThemeRect", 5, e_handle},
    {"UXTHEME.dll", "GetThemeMargins", 7, e_handle},
    {"UXTHEME.dll", "GetThemeIntList", 5, e_handle},
    {"UXTHEME.dll", "GetThemePropertyOrigin", 5, e_handle},
    {"UXTHEME.dll", "GetThemeFilename", 6, e_handle},
    {"UXTHEME.dll", "GetCurrentThemeName", 6, e_handle},
    {"UXTHEME.dll", "GetThemeDocumentationProperty", 4, e_handle},
    {"UXTHEME.dll", "DrawThemeParentBackground", 3, draw_parent_background},
    {"UXTHEME.dll", "BufferedPaintInit", 0, buffered_paint_unavailable},
    {"UXTHEME.dll", "BufferedPaintUnInit", 0, zero},
    {"WTSAPI32.dll", "WTSRegisterSessionNotification", 2, session_service_unavailable},
    {"WTSAPI32.dll", "WTSUnRegisterSessionNotification", 1, session_service_unavailable},
    {"WINSPOOL.DRV", "EnumPrintersW", 7, enum_printers},
    {"WINSPOOL.DRV", "GetDefaultPrinterW", 2, default_printer},
    {"WINSPOOL.DRV", "OpenPrinterW", 3, open_printer},
    {"WINSPOOL.DRV", "ClosePrinter", 1, yes},
    {"WINSPOOL.DRV", "DocumentPropertiesW", 6, document_properties},
    {"NETAPI32.dll", "NetWkstaGetInfo", 3, workstation_info},
    {"NETAPI32.dll", "NetApiBufferFree", 1, zero},
    {"MSVCR71.dll", "__set_app_type", ARGC_CDECL, crt_set_app_type},
    {"msvcrt.dll", "memcpy", ARGC_CDECL, crt_memcpy},
    {"msvcrt.dll", "memset", ARGC_CDECL, crt_memset},
    {"SHFOLDER.dll", "SHGetFolderPathW", 5, folder_path},
    {"SHELL32.dll", "SHGetFolderPathW", 5, folder_path},
    {"SHELL32.dll", "Shell_NotifyIconW", 2, yes},
    {"SHELL32.dll", "SHAppBarMessage", 2, zero},
    {"SHELL32.dll", "ShellExecuteW", 6, zero},
    {"SHLWAPI.dll", "StrCmpW", 2, s_StrCmpW},
    {"SHLWAPI.dll", "StrCmpNIW", 3, s_StrCmpNIW},
    {"SHLWAPI.dll", "StrToIntW", 1, s_StrToIntW},
    {"SHLWAPI.dll", "PathFindExtensionW", 1, s_PathFindExtensionW},
    {"SHLWAPI.dll", "PathFindFileNameW", 1, s_PathFindFileNameW},
    {"SHLWAPI.dll", "PathIsURLW", 1, s_PathIsURLW},
    {"SHLWAPI.dll", "UrlCanonicalizeW", 4, imports_unsupported},
    {"SHLWAPI.dll", "UrlCreateFromPathW", 4, imports_unsupported},
    {"SHLWAPI.dll", "PathCreateFromUrlW", 4, imports_unsupported},
    {"SHLWAPI.dll", "PathSearchAndQualifyW", 3, imports_unsupported},
    {"SHLWAPI.dll", "UrlUnescapeW", 4, imports_unsupported},
    {"SHLWAPI.dll", "UrlGetLocationW", 1, imports_unsupported},
    {"SHLWAPI.dll", "UrlIsW", 2, imports_unsupported},
    {"URLMON.dll", "URLDownloadToFileW", 5, url_download_to_file},
    // Symbol APIs have known ABIs but remain unsupported.
    {"DBGHELP.dll", "SymGetOptions", 0, imports_unsupported},
    {"DBGHELP.dll", "SymSetOptions", 1, imports_unsupported},
    {"DBGHELP.dll", "SymInitialize", 3, imports_unsupported},
    {"DBGHELP.dll", "SymCleanup", 1, imports_unsupported},
    {"hapdbg.dll", "?Initialize@hapdbg@@YA_NH@Z", ARGC_CDECL, hapdbg_noop},
    {"hapdbg.dll", "?WaitForConnection@hapdbg@@YA_NXZ", ARGC_CDECL, hapdbg_noop},
    {"hapdbg.dll", "?Uninitialize@hapdbg@@YAXXZ", ARGC_CDECL, hapdbg_noop},
    {"hapdbg.dll",
     "?OutputToDebugConsole@hapdbg@@YAXAAV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@"
     "std@@@Z",
     ARGC_CDECL, hapdbg_noop},
    {"hapdbg.dll",
     "?OutputToErrConsole@hapdbg@@YAXAAV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@"
     "std@@@Z",
     ARGC_CDECL, hapdbg_noop},
};
} // namespace
void misc_dlls_register() {
    imports_register(shims, sizeof shims / sizeof shims[0]);
}
