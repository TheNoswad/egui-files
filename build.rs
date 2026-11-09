//! Collects what the About window shows: the git commit and toolchain the app
//! was built from, and every crate compiled into it with its license texts.
//! On Windows, also puts the icon in the .exe.

// clippy.toml's disallowed items are for the browser; this runs on the build
// machine.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    env,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    for path in ["Cargo.toml", "Cargo.lock", "src"] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    write_if_changed(&out_dir.join("build_info.rs"), &build_info(&manifest_dir));
    let credits = credits(&manifest_dir).unwrap_or_else(|e| {
        println!("cargo:warning=no third-party credits in the About window: {e}");
        "pub static CRATES: &[Crate] = &[];\npub static TEXTS: &[&str] = &[];\n".into()
    });
    write_if_changed(&out_dir.join("credits.rs"), &credits);

    // The target's, not the build machine's: Windows is cross-compiled.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/icon/egui-files.ico");
        embed_resource::compile("build-support/egui-files.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}

/// Leaves the file alone when nothing changed, so it doesn't look modified.
fn write_if_changed(path: &Path, contents: &str) {
    if fs::read_to_string(path).ok().as_deref() != Some(contents) {
        fs::write(path, contents).unwrap();
    }
}

/// A command's trimmed stdout, if it succeeded.
fn run(program: impl AsRef<std::ffi::OsStr>, args: &[&str], dir: &Path) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn build_info(dir: &Path) -> String {
    let mut out = String::new();
    let git = |args: &[&str]| run("git", args, dir);

    match git(&["log", "-1", "--format=%H%n%h%n%ct%n%at%n%an%n%s"]) {
        Some(log) => {
            if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
                for path in ["HEAD", "index", "packed-refs", "refs"] {
                    println!("cargo:rerun-if-changed={git_dir}/{path}");
                }
            }
            let mut lines = log.lines();
            let mut next = || lines.next().unwrap_or_default().to_owned();
            let (hash, short_hash, commit_time, author_time, author, subject) =
                (next(), next(), next(), next(), next(), next());
            let status = git(&["status", "--porcelain"]).unwrap_or_default();
            let (untracked, modified): (Vec<_>, Vec<_>) =
                status.lines().partition(|l| l.starts_with("??"));
            let remote =
                git(&["config", "--get", "remote.origin.url"]).map(|url| strip_credentials(&url));
            writeln!(
                out,
                "pub const GIT: Option<Git> = Some(Git {{
    hash: {hash:?},
    short_hash: {short_hash:?},
    commit_time: {commit_time},
    author_time: {author_time},
    author: {author:?},
    subject: {subject:?},
    branch: {branch:?},
    describe: {describe:?},
    commit_count: {count},
    modified_files: {modified},
    untracked_files: {untracked},
    remote: {remote:?},
}});",
                branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default(),
                describe = git(&["describe", "--tags", "--always", "--dirty"]).unwrap_or_default(),
                count = git(&["rev-list", "--count", "HEAD"])
                    .and_then(|c| c.parse::<u64>().ok())
                    .unwrap_or(0),
                modified = modified.len(),
                untracked = untracked.len(),
            )
            .unwrap();
        }
        None => out.push_str("pub const GIT: Option<Git> = None;\n"),
    }

    let rustc = env::var_os("RUSTC").unwrap_or("rustc".into());
    let rustc_info = run(&rustc, &["-vV"], dir).unwrap_or_default();
    let field = |name: &str| {
        rustc_info
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let cargo = env::var_os("CARGO").unwrap_or("cargo".into());
    let built = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64)
        });
    let var = |name: &str| env::var(name).unwrap_or_default();
    writeln!(
        out,
        "pub const RUSTC: &str = {:?};
pub const LLVM: &str = {:?};
pub const CARGO: &str = {:?};
pub const PROFILE: &str = {:?};
pub const OPT_LEVEL: &str = {:?};
pub const TARGET: &str = {:?};
pub const HOST: &str = {:?};
pub const BUILT: i64 = {built};",
        rustc_info.lines().next().unwrap_or_default(),
        field("LLVM version:"),
        run(&cargo, &["-V"], dir).unwrap_or_default(),
        var("PROFILE"),
        var("OPT_LEVEL"),
        var("TARGET"),
        var("HOST"),
    )
    .unwrap();
    out
}

/// `https://user:token@host/repo` → `https://host/repo`.
fn strip_credentials(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('/') {
            Some((host, path)) => {
                let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
                format!("{scheme}://{host}/{path}")
            }
            None => url.to_owned(),
        },
        None => url.to_owned(),
    }
}

/// Files that a crate ships its licenses in, besides the usual LICENSE*,
/// COPYING* and NOTICE* at its root: licenses of what it bundles.
const BUNDLED_LICENSES: &[(&str, &str)] = &[
    ("epaint_default_fonts", "fonts"),
    ("unrar_sys", "vendor/unrar/license.txt"),
];

struct Package {
    name: String,
    version: String,
    license: String,
    authors: Vec<String>,
    description: String,
    repository: String,
    homepage: String,
    dir: PathBuf,
    license_file: Option<String>,
    proc_macro: bool,
}

fn credits(dir: &Path) -> Result<String, String> {
    let cargo = env::var_os("CARGO").unwrap_or("cargo".into());
    let target = env::var("TARGET").map_err(|e| e.to_string())?;
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--offline"])
        .args(["--filter-platform", &target])
        .current_dir(dir)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let metadata: Value = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    let str_of = |v: &Value| v.as_str().unwrap_or_default().to_owned();

    let mut packages = HashMap::new();
    for p in metadata["packages"].as_array().into_iter().flatten() {
        let manifest = PathBuf::from(str_of(&p["manifest_path"]));
        packages.insert(
            str_of(&p["id"]),
            Package {
                name: str_of(&p["name"]),
                version: str_of(&p["version"]),
                license: str_of(&p["license"]),
                authors: p["authors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(str_of)
                    .collect(),
                description: str_of(&p["description"])
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                repository: str_of(&p["repository"]),
                homepage: str_of(&p["homepage"]),
                dir: manifest.parent().unwrap().to_owned(),
                license_file: p["license_file"].as_str().map(str::to_owned),
                proc_macro: p["targets"].as_array().into_iter().flatten().any(|t| {
                    t["kind"]
                        .as_array()
                        .is_some_and(|k| k.iter().any(|k| k == "proc-macro"))
                }),
            },
        );
    }

    // (dependency, normal) edges per package. The root's dev-dependencies
    // are left out; other packages' aren't resolved at all.
    let root = str_of(&metadata["resolve"]["root"]);
    let mut edges: HashMap<String, Vec<(String, bool)>> = HashMap::new();
    for node in metadata["resolve"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let id = str_of(&node["id"]);
        let deps = node["deps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| {
                let kinds: Vec<&Value> = d["dep_kinds"].as_array().into_iter().flatten().collect();
                let normal = kinds.iter().any(|k| k["kind"].is_null());
                let build = kinds.iter().any(|k| k["kind"] == "build");
                (normal || build).then(|| (str_of(&d["pkg"]), normal))
            });
        edges.insert(id, deps.collect());
    }

    // In the app: reached through normal dependencies, not through a
    // proc-macro or build script. Everything else only runs while building.
    let reach = |runtime_only: bool| {
        let mut seen = HashSet::new();
        let mut stack = vec![root.clone()];
        while let Some(id) = stack.pop() {
            if (runtime_only && packages[&id].proc_macro) || !seen.insert(id.clone()) {
                continue;
            }
            for (dep, normal) in edges.get(&id).into_iter().flatten() {
                if *normal || !runtime_only {
                    stack.push(dep.clone());
                }
            }
        }
        seen.remove(&root);
        seen
    };
    let runtime = reach(true);
    let all = reach(false);
    let direct: HashSet<&String> = edges[&root].iter().map(|(d, _)| d).collect();
    let mut used_by: HashMap<&String, BTreeSet<&str>> = HashMap::new();
    for (id, deps) in &edges {
        if all.contains(id) || *id == root {
            for (dep, _) in deps {
                used_by.entry(dep).or_default().insert(&packages[id].name);
            }
        }
    }

    let mut texts: Vec<String> = Vec::new();
    let mut text_ids: HashMap<String, usize> = HashMap::new();
    let mut intern = |text: String| {
        let text = text.replace("\r\n", "\n").trim_end().to_owned() + "\n";
        *text_ids.entry(text.clone()).or_insert_with(|| {
            texts.push(text);
            texts.len() - 1
        })
    };

    let mut ids: Vec<&String> = all.iter().collect();
    ids.sort_by(|a, b| {
        let (a, b) = (&packages[*a], &packages[*b]);
        (a.name.to_lowercase(), &a.version).cmp(&(b.name.to_lowercase(), &b.version))
    });
    let files: Vec<Vec<(String, PathBuf)>> =
        ids.iter().map(|id| license_files(&packages[*id])).collect();

    // For crates that ship no license file: a copy of the Apache license
    // from another crate (its text is always the same).
    let apache = files
        .iter()
        .flatten()
        .filter(|(name, _)| name.to_uppercase().starts_with("LICENSE-APACHE"))
        .filter_map(|(_, path)| fs::read_to_string(path).ok())
        .fold(HashMap::<String, usize>::new(), |mut counts, text| {
            *counts.entry(text).or_default() += 1;
            counts
        })
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(text, _)| text);

    let mut out = String::from("pub static CRATES: &[Crate] = &[\n");
    for (id, files) in ids.iter().zip(files) {
        let p = &packages[*id];
        let mut shipped = Vec::new();
        for (name, path) in files {
            if let Ok(text) = fs::read(&path) {
                shipped.push((name, intern(String::from_utf8_lossy(&text).into_owned())));
            }
        }
        // Licenses of what the crate bundles don't cover the crate itself.
        let standard = !shipped.iter().any(|(name, _)| !name.contains('/'));
        if standard {
            let names = p
                .authors
                .iter()
                .map(|a| a.split(" <").next().unwrap_or(a).trim())
                .collect::<Vec<_>>()
                .join(", ");
            let holders = if names.is_empty() {
                format!("the {} developers", p.name)
            } else {
                names
            };
            if mentions(&p.license, "MIT") {
                shipped.push((
                    "MIT (standard text)".into(),
                    intern(MIT.replace("{holders}", &holders)),
                ));
            }
            if mentions(&p.license, "BSD-3-Clause") {
                shipped.push((
                    "BSD-3-Clause (standard text)".into(),
                    intern(BSD_3.replace("{holders}", &holders)),
                ));
            }
            if mentions(&p.license, "Apache-2.0")
                && let Some(apache) = &apache
            {
                shipped.push(("Apache-2.0 (standard text)".into(), intern(apache.clone())));
            }
        }
        let used_by: Vec<&str> = used_by
            .get(id)
            .into_iter()
            .flatten()
            .copied()
            .filter(|n| *n != env::var("CARGO_PKG_NAME").unwrap())
            .collect();
        writeln!(
            out,
            "    Crate {{ name: {:?}, version: {:?}, license: {:?}, authors: &{:?}, \
             description: {:?}, repository: {:?}, homepage: {:?}, direct: {}, \
             in_app: {}, used_by: &{:?}, files: &{:?}, standard_text: {standard} }},",
            p.name,
            p.version,
            p.license,
            p.authors,
            p.description,
            p.repository,
            p.homepage,
            direct.contains(id),
            runtime.contains(*id),
            used_by,
            shipped,
        )
        .unwrap();
    }
    out.push_str("];\n\npub static TEXTS: &[&str] = &[\n");
    for text in &texts {
        writeln!(out, "    {text:?},").unwrap();
    }
    out.push_str("];\n");
    Ok(out)
}

/// Whether an SPDX expression names a license (and not just one starting
/// the same, like MIT-0).
fn mentions(expression: &str, license: &str) -> bool {
    expression
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '.'))
        .any(|id| id == license)
}

/// The crate's license files, as (name shown, path), in name order.
fn license_files(p: &Package) -> Vec<(String, PathBuf)> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let mut add = |name: String, path: PathBuf| {
        if path.is_file() && !files.iter().any(|(_, p)| *p == path) {
            files.push((name, path));
        }
    };
    if let Ok(entries) = fs::read_dir(&p.dir) {
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| {
                let n = n.to_lowercase();
                [
                    "license",
                    "licence",
                    "copying",
                    "notice",
                    "unlicense",
                    "copyright",
                ]
                .iter()
                .any(|prefix| n.starts_with(prefix))
            })
            .collect();
        names.sort();
        for name in names {
            add(name.clone(), p.dir.join(name));
        }
    }
    if let Some(file) = &p.license_file {
        add(file.clone(), p.dir.join(file));
    }
    for (_, rel) in BUNDLED_LICENSES.iter().filter(|(name, _)| *name == p.name) {
        let path = p.dir.join(rel);
        if path.is_dir() {
            let mut names: Vec<String> = fs::read_dir(&path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".txt"))
                .collect();
            names.sort();
            for name in names {
                add(format!("{rel}/{name}"), path.join(name));
            }
        } else {
            add(rel.to_string(), path);
        }
    }
    files
}

const BSD_3: &str = "BSD 3-Clause License

Copyright (c) {holders}

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived from
   this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS \"AS IS\"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
";

const MIT: &str = "MIT License

Copyright (c) {holders}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the \"Software\"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
";
