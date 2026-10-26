use std::{process::Command,path::PathBuf};
fn main(){let out=PathBuf::from(std::env::var("OUT_DIR").unwrap());
let r=Command::new("cargo").args(["run","--manifest-path","tools/helper/Cargo.toml","--offline","--quiet"]).env("CARGO_TARGET_DIR",out.join("helper-target")).output().unwrap();
assert!(r.status.success(),"{}",String::from_utf8_lossy(&r.stderr));
std::fs::write(out.join("helper.stderr"),&r.stderr).unwrap();
std::fs::write(out.join("generated.rs"),format!("{:?}",String::from_utf8(r.stdout).unwrap().trim())).unwrap();
println!("cargo:rerun-if-changed=tools/helper/src/main.rs");}
