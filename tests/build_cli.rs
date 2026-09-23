//! `cargo hole build`, driven end-to-end as the user runs it.
//!
//! The point of the command is that the *generated* code is what gets compiled,
//! while the user's source is left alone. Neither half of that can be checked
//! without the real binary and a real cargo, so these tests do both: they assert
//! the source still has its `todo!()`, and they run the produced binary to prove
//! the generated code was the code that built.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// The `cargo-hole` binary under test, as built alongside this test.
fn cargo_hole() -> PathBuf {
    let mut path = std::env::current_exe().expect("test exe");
    path.pop(); // deps/
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("cargo-hole")
}

/// The workspace-local cargo home, so nothing touches a read-only `~/.cargo`.
fn cargo_home() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".cargo")
}

/// A crate whose two files both have holes, with a `main` that prints what the
/// generated code returns -- so running the binary is proof the artifact built.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/build-cli-tests")
            .join(format!(
                "{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"buildcli\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [[bin]]\nname = \"buildcli\"\npath = \"src/main.rs\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/main.rs"),
            "mod helper;\nfn main() {\n    println!(\"{} {}\", helper::greet(), answer());\n}\n\
             pub fn answer() -> i64 {\n    todo!(\"spec: return 42\")\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/helper.rs"),
            "pub fn greet() -> String {\n    todo!(\"spec: return hi\")\n}\n",
        )
        .unwrap();
        Fixture { dir }
    }

    fn main_rs(&self) -> String {
        std::fs::read_to_string(self.dir.join("src/main.rs")).unwrap()
    }

    fn helper_rs(&self) -> String {
        std::fs::read_to_string(self.dir.join("src/helper.rs")).unwrap()
    }

    /// Write the artifacts a `fill` would have produced. Only the two files that
    /// have holes get one, which is the shape the real store has.
    fn write_artifacts(&self, answer: &str, greeting: &str) {
        let store = self.dir.join(".cargo-hole/patch/src");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(
            store.join("main.rs"),
            format!(
                "mod helper;\nfn main() {{\n    println!(\"{{}} {{}}\", helper::greet(), answer());\n}}\n\
                 pub fn answer() -> i64 {{\n    {answer}\n}}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            store.join("helper.rs"),
            format!("pub fn greet() -> String {{\n    {greeting}\n}}\n"),
        )
        .unwrap();
    }

    fn shadow_dir(&self) -> PathBuf {
        self.dir.join(".cargo-hole/build")
    }

    /// Run `build` with the given extra arguments.
    fn build(&self, extra: &[&str]) -> std::process::Output {
        Command::new(cargo_hole())
            .arg("build")
            .arg("--path")
            .arg(&self.dir)
            .args(extra)
            .env("CARGO_HOLE_OFFLINE", "1")
            .env("CARGO_HOME", cargo_home())
            .output()
            .expect("run cargo-hole build")
    }

    /// Run the binary the shadow build produced, if there is one.
    fn run_built_binary(&self, profile: &str) -> Option<String> {
        let bin = self
            .shadow_dir()
            .join("target")
            .join(profile)
            .join("buildcli");
        if !bin.exists() {
            return None;
        }
        let out = Command::new(&bin).output().expect("run the built binary");
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// The whole point: generated code builds and runs, and the source is untouched.
#[test]
fn build_compiles_generated_code_without_touching_the_source() {
    let fx = Fixture::new("builds");
    fx.write_artifacts("42", "\"hi\".to_string()");
    let before_main = fx.main_rs();
    let before_helper = fx.helper_rs();

    let out = fx.build(&[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the build should succeed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Running the binary proves the *generated* code was what compiled: the
    // source returns `todo!()`, which would panic.
    assert_eq!(
        fx.run_built_binary("debug").as_deref(),
        Some("hi 42"),
        "the generated code did not make it into the build"
    );

    // And not one byte of the user's crate was rewritten.
    assert_eq!(fx.main_rs(), before_main, "src/main.rs was modified");
    assert_eq!(fx.helper_rs(), before_helper, "src/helper.rs was modified");
    assert!(
        fx.main_rs().contains("todo!"),
        "the hole was patched in place"
    );
    assert!(
        !fx.dir.join("src/main.rs.cargo-hole.bak").exists(),
        "a backup was left behind"
    );
}

/// The shadow has to carry the files that have no holes, or the crate will not
/// compile -- a hole-free module is still part of the crate.
#[test]
fn files_without_holes_are_carried_into_the_build() {
    let fx = Fixture::new("noholes");
    std::fs::write(
        fx.dir.join("src/plain.rs"),
        "pub fn plain() -> u8 {\n    7\n}\n",
    )
    .unwrap();
    // `main.rs` refers to it, so a shadow that dropped it could not build.
    std::fs::write(
        fx.dir.join("src/main.rs"),
        "mod helper;\nmod plain;\nfn main() {\n    println!(\"{} {} {}\", helper::greet(), \
         answer(), plain::plain());\n}\npub fn answer() -> i64 {\n    todo!(\"spec: return 42\")\n}\n",
    )
    .unwrap();
    fx.write_artifacts("42", "\"hi\".to_string()");
    // The artifact must match the edited main.rs, so rewrite it with the module.
    std::fs::write(
        fx.dir.join(".cargo-hole/patch/src/main.rs"),
        "mod helper;\nmod plain;\nfn main() {\n    println!(\"{} {} {}\", helper::greet(), \
         answer(), plain::plain());\n}\npub fn answer() -> i64 {\n    42\n}\n",
    )
    .unwrap();

    let out = fx.build(&[]);
    assert!(
        out.status.success(),
        "a hole-free module was not carried over:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fx.run_built_binary("debug").as_deref(),
        Some("hi 42 7"),
        "the hole-free module's code is missing from the build"
    );
}

/// A second build must reuse the tree, or the incremental cache never applies and
/// every build is a full rebuild.
#[test]
fn a_second_build_reuses_the_tree() {
    let fx = Fixture::new("reuse");
    fx.write_artifacts("42", "\"hi\".to_string()");

    let first = fx.build(&[]);
    assert!(first.status.success());
    let first_out = String::from_utf8_lossy(&first.stdout);
    assert!(
        first_out.contains("copied"),
        "the first build should assemble the tree:\n{first_out}"
    );

    let second = fx.build(&[]);
    assert!(second.status.success());
    let second_out = String::from_utf8_lossy(&second.stdout);
    assert!(
        second_out.contains("0 file(s) copied"),
        "the second build rewrote the tree, so nothing is incremental:\n{second_out}"
    );
}

/// `--clean` throws the tree away, and must not take the crate with it.
#[test]
fn clean_rebuilds_from_scratch_without_harming_the_source() {
    let fx = Fixture::new("clean");
    fx.write_artifacts("42", "\"hi\".to_string()");
    let before = fx.main_rs();

    fx.build(&[]);
    assert!(fx.shadow_dir().exists());

    let out = fx.build(&["--clean", "--dry-run"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("removed"),
        "--clean did not report: {stdout}"
    );

    assert_eq!(fx.main_rs(), before, "clean damaged the source");
    assert!(
        fx.dir.join("src/helper.rs").exists(),
        "clean removed the source"
    );
    assert!(
        fx.dir.join("Cargo.toml").exists(),
        "clean removed the manifest"
    );
    assert!(
        fx.dir.join(".cargo-hole/patch/src/main.rs").exists(),
        "clean removed the artifacts"
    );
}

/// A build tree with no artifacts is legitimate: it builds the crate as-is, but
/// must say that the holes are still unelaborated, because `todo!()` compiles and
/// the build would otherwise look like a full success.
#[test]
fn an_unfilled_crate_builds_but_says_the_holes_are_open() {
    let fx = Fixture::new("unfilled");

    let out = fx.build(&["--dry-run"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("unelaborated"),
        "the open holes were not reported:\n{stderr}"
    );
    assert!(
        stderr.contains("cargo hole fill"),
        "the report should say what to do:\n{stderr}"
    );
    // `--dry-run` assembles the tree but must not run cargo.
    assert!(stdout.contains("would run"), "{stdout}");
}

/// A failing build propagates its exit status, so a script wrapping this command
/// sees what `cargo build` would have told it.
#[test]
fn a_failing_build_propagates_a_nonzero_status() {
    let fx = Fixture::new("failing");
    // An answer that does not compile.
    fx.write_artifacts("no_such_function()", "\"hi\".to_string()");

    let out = fx.build(&[]);

    assert!(
        !out.status.success(),
        "a broken artifact must not report success"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no_such_function") || stderr.contains("E0425"),
        "rustc's error should reach the user:\n{stderr}"
    );
    // The source is still intact even though the build failed.
    assert!(fx.main_rs().contains("todo!"), "source was modified");
}

/// Flags after the command go to cargo, so the command does not have to know
/// every cargo flag.
#[test]
fn trailing_arguments_are_forwarded_to_cargo() {
    let fx = Fixture::new("forward");
    fx.write_artifacts("42", "\"hi\".to_string()");

    let out = fx.build(&["--release"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("release") || stderr.contains("Finished"),
        "cargo did not run a release build:\n{stderr}"
    );
    assert!(
        fx.run_built_binary("release").is_some(),
        "no release binary was produced, so --release was not forwarded"
    );
}

/// `--build-dir` puts the tree somewhere else, and the source is still untouched.
#[test]
fn a_custom_build_dir_is_honoured() {
    let fx = Fixture::new("custom-dir");
    fx.write_artifacts("42", "\"hi\".to_string()");
    let elsewhere = fx
        .dir
        .parent()
        .unwrap()
        .join(format!("custom-build-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&elsewhere);

    let out = fx.build(&["--build-dir", elsewhere.to_str().unwrap(), "--dry-run"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        elsewhere.join("Cargo.toml").exists(),
        "the custom build dir was not used"
    );
    assert!(
        !fx.shadow_dir().exists(),
        "the default location was used anyway"
    );
    let _ = std::fs::remove_dir_all(&elsewhere);
}

/// Repeated builds must not nest a copy of the build tree inside itself.
///
/// The default tree lives *inside* `.cargo-hole/`, which is also where the
/// artifacts are read from. A sync that enumerated the store without skipping its
/// own output would copy `build/` into `build/build/`, and the artifact count
/// would grow on every run.
#[test]
fn repeated_builds_do_not_nest_a_copy_of_themselves() {
    let fx = Fixture::new("no-nesting");
    fx.write_artifacts("42", "\"hi\".to_string()");

    let mut counts = Vec::new();
    for _ in 0..3 {
        let out = fx.build(&["--dry-run"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Read "N artifact(s) overlaid" out of the summary line.
        let head = stdout
            .split(" artifact(s) overlaid")
            .next()
            .unwrap_or_else(|| panic!("no artifact count in: {stdout}"));
        let count: usize = head
            .rsplit(", ")
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("cannot parse the artifact count from: {head}"));
        counts.push(count);
    }

    assert_eq!(counts, vec![2, 2, 2], "the build tree nested into itself");
    assert!(
        !fx.shadow_dir().join("build").exists(),
        "a copy of the build tree was nested inside it"
    );
}

/// A path that would make `--clean` destructive is refused before anything runs.
#[test]
fn a_destructive_build_dir_is_refused() {
    let fx = Fixture::new("destructive");
    let parent = fx.dir.parent().unwrap().to_path_buf();

    let out = fx.build(&["--build-dir", parent.to_str().unwrap()]);

    assert!(
        !out.status.success(),
        "a destructive build dir was accepted"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("contains the crate root"),
        "the refusal was not explained:\n{stderr}"
    );
    // The crate is still there, which is the entire point.
    assert!(fx.dir.join("Cargo.toml").exists(), "the crate was deleted");
    assert!(fx.main_rs().contains("todo!"), "the source was damaged");
}
