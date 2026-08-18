//! Extension → gpui-component highlighter language name. Only names that the
//! pinned rev's `highlighter::Language` actually registers are returned —
//! anything unmapped renders as plain text (the registry's own fallback).

pub fn language_for_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "rust",
        "py" | "pyw" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "html" | "htm" => "html",
        "css" | "scss" => "css",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "go" => "go",
        "rb" => "ruby",
        "php" => "php",
        "lua" => "lua",
        "sh" | "bash" | "zsh" => "bash",
        "sql" => "sql",
        "zig" => "zig",
        "swift" => "swift",
        "scala" => "scala",
        "proto" => "proto",
        "cmake" => "cmake",
        "mk" => "make",
        "diff" | "patch" => "diff",
        "ex" | "exs" => "elixir",
        "erb" => "erb",
        "ejs" => "ejs",
        "astro" => "astro",
        "svelte" => "svelte",
        "graphql" | "gql" => "graphql",
        "md" | "markdown" => "markdown",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_unknown() {
        assert_eq!(language_for_ext("rs"), Some("rust"));
        assert_eq!(language_for_ext("yml"), Some("yaml"));
        assert_eq!(language_for_ext("tsx"), Some("tsx"));
        assert_eq!(language_for_ext("xml"), None);
        assert_eq!(language_for_ext("vue"), None);
        assert_eq!(language_for_ext(""), None);
    }
}
