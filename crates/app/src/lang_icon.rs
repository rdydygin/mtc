//! Monochrome language logos for code blocks.
//!
//! Logos come from Simple Icons (CC0, see `assets/lang/LICENSE.md`) and are
//! drawn like any icon, tinted with the surrounding text color. A language
//! without a logo gets a generic code glyph.

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;

/// Embedded logo files, by Simple Icons slug.
pub const LOGOS: &[(&str, &[u8])] = &[
    ("c", include_bytes!("../assets/lang/c.svg")),
    ("clojure", include_bytes!("../assets/lang/clojure.svg")),
    ("cplusplus", include_bytes!("../assets/lang/cplusplus.svg")),
    ("css", include_bytes!("../assets/lang/css.svg")),
    ("dart", include_bytes!("../assets/lang/dart.svg")),
    ("docker", include_bytes!("../assets/lang/docker.svg")),
    ("dotnet", include_bytes!("../assets/lang/dotnet.svg")),
    ("elixir", include_bytes!("../assets/lang/elixir.svg")),
    ("erlang", include_bytes!("../assets/lang/erlang.svg")),
    ("git", include_bytes!("../assets/lang/git.svg")),
    ("gnubash", include_bytes!("../assets/lang/gnubash.svg")),
    ("go", include_bytes!("../assets/lang/go.svg")),
    ("graphql", include_bytes!("../assets/lang/graphql.svg")),
    ("haskell", include_bytes!("../assets/lang/haskell.svg")),
    ("html5", include_bytes!("../assets/lang/html5.svg")),
    (
        "javascript",
        include_bytes!("../assets/lang/javascript.svg"),
    ),
    ("json", include_bytes!("../assets/lang/json.svg")),
    ("julia", include_bytes!("../assets/lang/julia.svg")),
    ("kotlin", include_bytes!("../assets/lang/kotlin.svg")),
    ("lua", include_bytes!("../assets/lang/lua.svg")),
    ("markdown", include_bytes!("../assets/lang/markdown.svg")),
    ("nginx", include_bytes!("../assets/lang/nginx.svg")),
    ("nixos", include_bytes!("../assets/lang/nixos.svg")),
    ("ocaml", include_bytes!("../assets/lang/ocaml.svg")),
    ("openjdk", include_bytes!("../assets/lang/openjdk.svg")),
    ("perl", include_bytes!("../assets/lang/perl.svg")),
    ("php", include_bytes!("../assets/lang/php.svg")),
    (
        "postgresql",
        include_bytes!("../assets/lang/postgresql.svg"),
    ),
    ("python", include_bytes!("../assets/lang/python.svg")),
    ("r", include_bytes!("../assets/lang/r.svg")),
    ("react", include_bytes!("../assets/lang/react.svg")),
    ("ruby", include_bytes!("../assets/lang/ruby.svg")),
    ("rust", include_bytes!("../assets/lang/rust.svg")),
    ("scala", include_bytes!("../assets/lang/scala.svg")),
    ("solidity", include_bytes!("../assets/lang/solidity.svg")),
    ("svelte", include_bytes!("../assets/lang/svelte.svg")),
    ("swift", include_bytes!("../assets/lang/swift.svg")),
    ("terraform", include_bytes!("../assets/lang/terraform.svg")),
    ("toml", include_bytes!("../assets/lang/toml.svg")),
    (
        "typescript",
        include_bytes!("../assets/lang/typescript.svg"),
    ),
    ("vuedotjs", include_bytes!("../assets/lang/vuedotjs.svg")),
    ("yaml", include_bytes!("../assets/lang/yaml.svg")),
    ("zig", include_bytes!("../assets/lang/zig.svg")),
];

/// The asset path prefix the logos are served under.
pub const PREFIX: &str = "lang/";

/// Map a fence language (`rs`, `py`, `c++`, …) to a logo slug.
fn slug(lang: &str) -> Option<&'static str> {
    let slug = match lang.trim().to_lowercase().as_str() {
        "rust" | "rs" => "rust",
        "python" | "py" | "python3" => "python",
        "javascript" | "js" | "mjs" | "cjs" | "node" => "javascript",
        "typescript" | "ts" => "typescript",
        "jsx" | "tsx" => "react",
        "go" | "golang" => "go",
        "java" => "openjdk",
        "kotlin" | "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cpp" | "c++" | "cc" | "cxx" | "hpp" => "cplusplus",
        "csharp" | "cs" | "c#" | "fsharp" | "fs" => "dotnet",
        "ruby" | "rb" => "ruby",
        "php" => "php",
        "bash" | "sh" | "shell" | "zsh" | "console" | "shellscript" => "gnubash",
        "html" | "htm" | "xhtml" => "html5",
        "css" | "scss" | "sass" | "less" => "css",
        "json" | "jsonc" | "json5" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "markdown" | "md" => "markdown",
        "docker" | "dockerfile" => "docker",
        "lua" => "lua",
        "dart" => "dart",
        "scala" => "scala",
        "haskell" | "hs" => "haskell",
        "elixir" | "ex" | "exs" => "elixir",
        "erlang" | "erl" => "erlang",
        "zig" => "zig",
        "r" => "r",
        "perl" | "pl" => "perl",
        "nix" => "nixos",
        "graphql" | "gql" => "graphql",
        "vue" => "vuedotjs",
        "svelte" => "svelte",
        "diff" | "patch" | "git" => "git",
        "terraform" | "hcl" | "tf" => "terraform",
        "ocaml" | "ml" => "ocaml",
        "clojure" | "clj" | "cljs" => "clojure",
        "julia" | "jl" => "julia",
        "solidity" | "sol" => "solidity",
        "nginx" => "nginx",
        "sql" | "psql" | "postgres" | "postgresql" | "pgsql" => "postgresql",
        _ => return None,
    };
    Some(slug)
}

/// The icon for a code block's language.
pub fn icon(lang: &str) -> Icon {
    match slug(lang) {
        Some(slug) => Icon::empty().path(format!("{PREFIX}{slug}.svg")),
        None => Icon::new(IconName::CodeXml),
    }
}

/// The embedded bytes for an asset path under [`PREFIX`].
pub fn load(path: &str) -> Option<&'static [u8]> {
    let slug = path.strip_prefix(PREFIX)?.strip_suffix(".svg")?;
    LOGOS
        .iter()
        .find(|(s, _)| *s == slug)
        .map(|(_, bytes)| *bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_alias_has_a_bundled_logo() {
        for lang in [
            "rs",
            "py",
            "ts",
            "go",
            "c++",
            "sh",
            "yml",
            "sql",
            "tsx",
            "dockerfile",
        ] {
            let slug = slug(lang).unwrap();
            assert!(
                load(&format!("{PREFIX}{slug}.svg")).is_some(),
                "{lang} -> {slug}"
            );
        }
        assert!(slug("brainfuck").is_none());
    }
}
