use std::{env, path::PathBuf};
fn main() {
    println!("cargo:rerun-if-changed=bridge/loader.c");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    assert_eq!(
        env::var("CARGO_CFG_TARGET_ENV").as_deref(),
        Ok("gnu"),
        "Linux QQ requires a GNU libc target"
    );
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("libqqtui-loader.so");
    let compiler = cc::Build::new().pic(true).get_compiler();
    let mut command = compiler.to_command();
    command.args(["-shared", "bridge/loader.c"]);
    println!("cargo:rerun-if-env-changed=LDFLAGS");
    if let Some(flags) = env::var_os("LDFLAGS") {
        let flags = flags.into_string().expect("LDFLAGS must be UTF-8");
        command.args(shlex::split(&flags).expect("Invalid quoting in LDFLAGS"));
    }
    let status = command
        .arg("-o")
        .arg(output)
        .arg("-ldl")
        .status()
        .expect("C compiler required for Linux QQ loader");
    assert!(
        status.success(),
        "Linux QQ loader compilation failed with {}: {status}",
        compiler.path().display()
    );
}
