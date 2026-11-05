//! Tiny std-only launcher: no GTK linkage and no dynamic library dependencies beyond libc.
use std::env;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn sibling_binary(name: &str) -> Option<PathBuf> {
    let current = env::current_exe().ok()?;
    let path = current.parent()?.join(name);
    path.is_file().then_some(path)
}

fn graphical_display() -> bool {
    env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
        || env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
}

fn find_binary(name: &str) -> OsString {
    sibling_binary(name)
        .map(PathBuf::into_os_string)
        .unwrap_or_else(|| OsString::from(name))
}

fn main() {
    let mut args = env::args_os().skip(1).collect::<Vec<_>>();
    if matches!(args.first().and_then(|arg| arg.to_str()), Some("--help") | Some("-h")) {
        println!("Salamander RS — jeden projekt, dvě rozhraní\nPoužití: salamander [--gui|--tui] [LEVÝ_ADRESÁŘ] [PRAVÝ_ADRESÁŘ]\nBez parametrů vybere GUI při dostupném DISPLAY / WAYLAND_DISPLAY, jinak TUI.\nNa server bez GTK stačí jen 'salamander' a 'salamander-rs-tui'.");
        return;
    }
    let mode = match args.first().and_then(|arg| arg.to_str()) {
        Some("--gui") => { args.remove(0); "gui" },
        Some("--tui") => { args.remove(0); "tui" },
        _ => if graphical_display() && sibling_binary("salamander-rs").is_some() { "gui" } else { "tui" },
    };
    let exe = if mode == "gui" { "salamander-rs" } else { "salamander-rs-tui" };
    let executable = find_binary(exe);
    let err = Command::new(&executable).args(&args).exec();
    eprintln!("Nelze spustit {}: {}.\nZkontroluj instalaci příslušného balíčku.", Path::new(&executable).display(), err);
    std::process::exit(127);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sibling_path_is_deterministic() {
        assert!(sibling_binary("not-a-salamander-binary").is_none());
    }
}
