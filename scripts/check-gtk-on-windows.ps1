# Type-checks (does not build or link) the GTK frontend on Windows, where GTK's development
# libraries and pkg-config are unavailable. The -sys crates' build scripts are told to skip
# pkg-config and emit a placeholder library name, which is enough for `cargo check`.
#
# Expected result: exactly two errors in notify.rs, from notify-rust APIs that only exist on
# Linux (`show_async`, `get_server_information`). Anything else is a real error. Borrow checking
# still runs for every other function despite those two, so this catches type and ownership
# mistakes. It does not replace `scripts/validate-linux.sh` on a real Linux host.
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$libs = 'GLIB_2_0', 'GOBJECT_2_0', 'GIO_2_0', 'GIO_WINDOWS_2_0', 'GIO_UNIX_2_0', 'GDK_PIXBUF_2_0',
    'PANGO', 'PANGOCAIRO', 'CAIRO', 'CAIRO_GOBJECT', 'GRAPHENE_GOBJECT_1_0', 'GTK4', 'LIBADWAITA_1',
    'HARFBUZZ', 'GMODULE_2_0'
foreach ($lib in $libs) {
    Set-Item "env:SYSTEM_DEPS_${lib}_NO_PKG_CONFIG" '1'
    Set-Item "env:SYSTEM_DEPS_${lib}_LIB" 'placeholder'
}
# Separate target dir: the placeholder link flags must never reach a real build's cache.
$env:CARGO_TARGET_DIR = 'target/gtk-check'
# Errors and non-deprecation warnings, each with its source location.
cargo check -p lilypad-gtk --tests --locked @args 2>&1 |
    ForEach-Object { "$_" } |
    Select-String -Pattern '^(error|warning)(\[\w+\])?: (?!use of deprecated)' -Context 0, 1 |
    ForEach-Object { $_.Line; $_.Context.PostContext }
