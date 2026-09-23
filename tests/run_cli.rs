//! `cargo hole run`, driven end-to-end as the user runs it.
//!
//! The command is `cargo run` for the generated tree, so the tests are about the
//! two things that make it more than `build && ./binary`: the program must see the
//! *crate's* working directory rather than the build tree's, and its exit status
//! must reach the caller unchanged. Both are invisible in a passing build, so both
//! are asserted from the program's own output.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

fn cargo_hole() -> PathBuf {
    let mut path = std::env::current_exe().expect("test exe");
    path.pop(); // deps/
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("cargo-hole")
}

fn cargo_home() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".cargo")
}

/// A crate whose `main` reports the things `run` has to get right: the working
/// directory it was given, its own arguments, and a relative file it reads.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/run-cli-tests")
            .join(format!(
                "{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"runcli\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [[bin]]\nname = \"runcli\"\npath = \"src/main.rs\"\n\n[workspace]\n",
        )
        .unwrap();
        // A data file the program reads by relative path. Present in the crate, so
        // a wrong working directory is only visible through the printed path.
        std::fs::write(dir.join("data/input.txt"), "crate data\n").unwrap();
        std::fs::write(
            dir.join("src/main.rs"),
            "pub fn answer() -> i64 {\n    todo!(\"spec: return 42\")\n}\n",
        )
        .unwrap();
        Fixture { dir }
    }

    fn write_artifacts(&self, body: &str) {
        let store = self.dir.join(".cargo-hole/patch/src");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("main.rs"), body).unwrap();
    }

    /// An artifact whose program reports the working directory it was given, its
    /// own arguments, and a relative file it reads -- the three things `run` has to
    /// get right -- then exits with `code`.
    fn artifact_exiting(&self, code: i32) -> String {
        format!(
            "fn main() {{\n    \
             println!(\"cwd={{}}\", std::env::current_dir().unwrap().display());\n    \
             println!(\"args={{:?}}\", std::env::args().skip(1).collect::<Vec<_>>());\n    \
             match std::fs::read_to_string(\"data/input.txt\") {{\n        \
             Ok(s) => println!(\"data={{}}\", s.trim()),\n        \
             Err(e) => println!(\"data-error={{e}}\"),\n    }}\n    \
             std::process::exit({code});\n}}\n"
        )
    }

    fn run(&self, extra: &[&str]) -> std::process::Output {
        Command::new(cargo_hole())
            .arg("run")
            .arg("--path")
            .arg(&self.dir)
            .args(extra)
            .env("CARGO_HOLE_OFFLINE", "1")
            .env("CARGO_HOME", cargo_home())
            .output()
            .expect("run cargo-hole run")
    }
}

/// The generated code is what runs -- the source still has `todo!()`, which would
/// panic -- and the program's working directory is the crate, not the build tree.
#[test]
fn run_executes_the_generated_code_in_the_crates_directory() {
    let fx = Fixture::new("basic");
    fx.write_artifacts(&fx.artifact_exiting(0));

    let out = fx.run(&[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "run failed:\n{stderr}");

    assert!(
        stdout.contains(&format!("cwd={}", fx.dir.display())),
        "the program did not run in the crate directory, so a relative path would \
         resolve against the build tree:\n{stdout}"
    );
    // Only the program's own report is checked, not the whole output: this
    // command's progress line names the build tree too, and matching that would
    // make the test pass for the wrong reason.
    let cwd_line = stdout
        .lines()
        .find(|l| l.starts_with("cwd="))
        .expect("the program should have printed its directory");
    assert!(
        !cwd_line.contains(".cargo-hole/build"),
        "the program was given the build tree as its directory: {cwd_line}"
    );
    assert!(
        stdout.contains("data=crate data"),
        "a relative file did not resolve against the crate:\n{stdout}"
    );
}

/// The program's exit status must reach the caller, or a script wrapping this
/// command cannot tell success from failure.
#[test]
fn the_programs_exit_status_reaches_the_caller() {
    let fx = Fixture::new("exit-code");
    fx.write_artifacts(&fx.artifact_exiting(3));

    let out = fx.run(&[]);

    assert_eq!(
        out.status.code(),
        Some(3),
        "the program's exit status was not propagated: {:?}",
        out.status
    );
}

/// A tree that does not compile must fail loudly rather than reporting success.
#[test]
fn a_broken_artifact_fails_and_shows_rustc() {
    let fx = Fixture::new("broken");
    fx.write_artifacts("fn main() { no_such_function(); }\n");

    let out = fx.run(&[]);

    assert!(!out.status.success(), "a broken tree reported success");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no_such_function") || stderr.contains("E0425"),
        "rustc's error should reach the user:\n{stderr}"
    );
}

/// Arguments after `--` go to the program, not to cargo.
#[test]
fn arguments_after_the_separator_reach_the_program() {
    let fx = Fixture::new("program-args");
    fx.write_artifacts(&fx.artifact_exiting(0));

    let out = fx.run(&["--", "alpha", "--beta"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(r#"args=["alpha", "--beta"]"#),
        "the program did not receive its arguments:\n{stdout}"
    );
}

/// `--help` after the separator belongs to the program. If cargo consumed it, the
/// user would get cargo's help instead of their own.
#[test]
fn help_after_the_separator_belongs_to_the_program() {
    let fx = Fixture::new("program-help");
    fx.write_artifacts(&fx.artifact_exiting(0));

    let out = fx.run(&["--", "--help"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(r#"args=["--help"]"#),
        "the flag was swallowed instead of reaching the program:\n{stdout}"
    );
}

/// Cargo flags before the separator go to cargo. `--release` is the clearest one
/// to observe: it changes where the binary lands.
#[test]
fn cargo_flags_before_the_separator_reach_cargo() {
    let fx = Fixture::new("cargo-args");
    fx.write_artifacts(&fx.artifact_exiting(0));

    let out = fx.run(&["--release"]);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        fx.dir
            .join(".cargo-hole/build/target/release/runcli")
            .exists(),
        "the release binary was not produced, so --release did not reach cargo"
    );
}

/// `--dry-run` assembles the tree but must not build or run anything.
#[test]
fn dry_run_does_not_run_the_program() {
    let fx = Fixture::new("dry-run");
    fx.write_artifacts(&fx.artifact_exiting(0));

    let out = fx.run(&["--dry-run"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("would run"),
        "--dry-run should say what it would do:\n{stdout}"
    );
    assert!(
        !stdout.contains("cwd="),
        "the program ran despite --dry-run:\n{stdout}"
    );
}

/// Running must not touch the user's source, exactly as `build` must not.
#[test]
fn the_real_source_is_left_alone() {
    let fx = Fixture::new("source-safe");
    fx.write_artifacts(&fx.artifact_exiting(0));
    let before = std::fs::read_to_string(fx.dir.join("src/main.rs")).unwrap();

    fx.run(&[]);

    assert_eq!(
        std::fs::read_to_string(fx.dir.join("src/main.rs")).unwrap(),
        before,
        "run modified the user's source"
    );
    assert!(
        !fx.dir.join("target").exists(),
        "the real target directory was written to, so the build was not isolated"
    );
}

/// An unfilled crate still runs -- `todo!()` compiles -- but the user must be told
/// the holes are open, or a panic at runtime would be the first they hear of it.
#[test]
fn a_crate_with_open_holes_runs_but_says_so() {
    let fx = Fixture::new("open-holes");

    let out = fx.run(&["--dry-run"]);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unelaborated"),
        "the open holes were not reported:\n{stderr}"
    );
}
