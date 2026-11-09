use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}
fn markdown(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut stack = vec![root.join("docs")];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p)
            } else if p.extension().is_some_and(|s| s == "md") {
                paths.push(p);
            }
        }
    }
    for p in [
        "README.md",
        "CONTRIBUTING.md",
        ".github/PULL_REQUEST_TEMPLATE.md",
        "tests/e2e/README.md",
    ] {
        paths.push(root.join(p));
    }
    paths
}

#[test]
fn active_documentation_links_and_anchors_resolve() {
    let root = root();
    let links = regex::Regex::new(r"\[[^\]]*\]\(([^\s)]+)\)").unwrap();
    let headings = regex::Regex::new(r"(?m)^#{1,6}\s+(.+)$").unwrap();
    let punctuation = regex::Regex::new(r"[^\w\- ]").unwrap();
    for path in markdown(&root) {
        let source = fs::read_to_string(&path).unwrap();
        for link in links.captures_iter(&source) {
            let target = &link[1];
            if ["https:", "http:", "mailto:", "#"]
                .iter()
                .any(|p| target.starts_with(p))
            {
                continue;
            }
            let (file, anchor) = target
                .split_once('#')
                .map(|(a, b)| (a, Some(b)))
                .unwrap_or((target, None));
            let destination = path.parent().unwrap().join(file);
            assert!(
                destination.exists(),
                "{} links to missing {target}",
                path.display()
            );
            if let Some(anchor) = anchor {
                if destination.extension().is_some_and(|s| s == "md") {
                    let content = fs::read_to_string(destination).unwrap();
                    let mut slugs = BTreeSet::new();
                    for heading in headings.captures_iter(&content) {
                        let base = punctuation
                            .replace_all(&heading[1].trim().to_lowercase(), "")
                            .replace(' ', "-");
                        let mut slug = base.clone();
                        let mut n = 0;
                        while slugs.contains(&slug) {
                            n += 1;
                            slug = format!("{base}-{n}");
                        }
                        slugs.insert(slug);
                    }
                    assert!(
                        slugs.contains(anchor),
                        "{} links to missing anchor {target}",
                        path.display()
                    );
                }
            }
        }
    }
}

#[test]
fn production_entrypoints_are_native_and_have_no_python_sources() {
    let root = root();
    for path in ["Dockerfile", "compose.yaml", "scripts/open-assets"] {
        let text = fs::read_to_string(root.join(path)).unwrap();
        assert!(
            !text.contains("python -m") && !text.contains("python3 -m"),
            "Python runtime entry remains in {path}"
        );
    }
    let mut stack = vec![root.join("scripts"), root.join("tests")];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p)
            } else {
                assert!(
                    !p.extension().is_some_and(|s| s == "py"),
                    "Python implementation remains: {}",
                    p.display()
                );
            }
        }
    }
    assert!(!root.join("open_assets").exists());
    assert!(!root.join("requirements.txt").exists());
    for e in fs::read_dir(root.join("config/systemd")).unwrap() {
        let path = e.unwrap().path();
        if path.extension().is_some_and(|s| s == "service") {
            assert!(
                !fs::read_to_string(&path).unwrap().contains("python"),
                "Python systemd entry remains: {}",
                path.display()
            );
        }
    }
}
