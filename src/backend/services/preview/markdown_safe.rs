//! Strip URL sinks out of a Markdown document before it is rendered.
//!
//! # The hole
//!
//! gpui-component's Markdown renderer turns every image into
//! `ImageNode { url: SharedUri }` and renders it with `img(url)`. gpui's
//! `impl From<SharedUri> for ImageSource` is unconditional — the `is_uri`
//! sniff exists only on the `&str`/`String` impls — so **every** image becomes
//! `Resource::Uri` and goes to `App::http_client`. Selecting a `.md` file in
//! the inspector is enough to fetch an attacker-chosen URL.
//!
//! A corollary worth stating plainly: because *every* image takes that path,
//! Markdown images have never actually rendered in PIKU and cannot. Removing
//! them is a no-op on pixels. That is what lets this coexist with "the UI does
//! not change".
//!
//! Links are the second sink. A click reaches `cx.open_url`, which is
//! `xdg-open` / `ShellExecute` — so a `file://`, `smb://`, or arbitrary
//! app-scheme URL in a document the user merely previewed is one click from
//! being handed to the shell. Click-gated, so milder than the automatic image
//! fetch, and in the same document.
//!
//! # Why mdast and not a regex
//!
//! `markdown::to_mdast` with [`ParseOptions::gfm`] is the *exact* parser and
//! configuration gpui-component uses. Using the same one means the spans line
//! up byte-for-byte and there is no parser-differential to exploit — a
//! hand-rolled scanner would have to independently get code fences, HTML
//! comments, reference definitions and entity escapes right, forever. The crate
//! is already in the lock file as a transitive dependency of the renderer, so
//! this adds no new supply chain.
//!
//! # Why not `ammonia`
//!
//! It is a browser XSS sanitizer. Its default allowlist *permits* `<img src>`
//! on http/https, so it does not address the threat here; it pulls in a large
//! `html5ever` surface; and the thing it does address — script execution — is
//! not something a native renderer does. Rejected on the merits, not on size.
//!
//! Note this file never parses HTML. `Node::Html` is the only route from
//! Markdown into the renderer's HTML path (and thus to its own image nodes), so
//! removing that node closes the HTML sink without knowing anything about HTML.

use markdown::mdast::Node;
use markdown::{ParseOptions, to_mdast};

/// Link schemes that may survive. Everything else is replaced by its own text.
///
/// `mailto` is here because it is the one non-web scheme people genuinely put
/// in READMEs. A scheme-less target (`#anchor`, `./doc.md`) is also allowed:
/// stripping those would visibly change ordinary documents, and they carry no
/// scheme for the shell to dispatch on.
const ALLOWED_SCHEMES: &[&str] = &["http", "https", "mailto"];

/// What a pass removed. Surfaced for logging, not for the UI — the point is
/// that the document renders the same either way.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MarkdownReport {
    pub images_removed: usize,
    pub html_removed: usize,
    pub links_defanged: usize,
}

impl MarkdownReport {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Maximum block-container nesting a document may have before it is refused.
///
/// Found the hard way: `a_deeply_nested_document_is_refused_rather_than_parsed`
/// started life asserting that neutralizing 20 000 nested blockquotes worked,
/// and instead **aborted the test process with a stack overflow**. The parse
/// itself survives a couple of thousand levels, so the limit is somewhere past
/// that — but the exact number does not matter, because the mitigation cannot
/// be "recover". A stack overflow is not a panic: it aborts, and `catch_unwind`
/// cannot see it.
///
/// This is why the refusal has to happen *before* the parse rather than around
/// it, and why it matters far beyond this module: **gpui-component parses the
/// same document with the same parser to render it.** A document that can
/// overflow our parse can overflow its parse too. Refusing here and rendering
/// the source as plain text keeps it away from both.
///
/// 64 is three or four orders of magnitude above any real document; the deepest
/// nesting in ordinary prose is a quoted reply chain, and those do not reach
/// ten.
const MAX_CONTAINER_DEPTH: usize = 64;

/// The outcome of neutralizing a document.
pub enum Neutralized {
    /// Safe to hand to the Markdown renderer.
    Safe(String, MarkdownReport),
    /// Do not render this as Markdown at all — show the source as plain text.
    /// Carries a static reason for the log.
    RenderAsPlainText(&'static str),
}

/// Rewrite `src` so that rendering it cannot construct a URL sink.
///
/// Guarantee, stated in the renderer's own terms: a [`Neutralized::Safe`]
/// result parses to a tree containing no `Image`, no `ImageReference`, and no
/// `Html` node, and every surviving `Link` has an allowed scheme. Pinned by
/// `a_neutralized_document_parses_to_a_tree_with_no_image_or_html_nodes`.
pub fn neutralize(src: &str) -> Neutralized {
    let depth = container_depth(src);
    if depth > MAX_CONTAINER_DEPTH {
        return Neutralized::RenderAsPlainText("nesting depth");
    }
    let Ok(_) = to_mdast(src, &ParseOptions::gfm()) else {
        return Neutralized::RenderAsPlainText("unparseable");
    };

    // Two passes on purpose. Removing images first means that when the link
    // pass reconstructs a defanged link's visible text, there is no image left
    // inside it to reintroduce. One pass would have to reason about a
    // replacement nested inside another replacement; two passes make it a
    // non-question.
    let (once, mut report) = strip_images_and_html(src);
    let (twice, links) = defang_links(&once);
    report.links_defanged = links;
    Neutralized::Safe(twice, report)
}

/// Greatest block-container nesting on any line, measured without parsing.
///
/// Deliberately crude and deliberately over-counting: it costs one linear scan
/// and only has to separate "ordinary document" from "pathological", not
/// reproduce CommonMark's container rules.
fn container_depth(src: &str) -> usize {
    let mut worst = 0;
    for line in src.lines() {
        let mut depth = 0;
        let mut spaces = 0;
        for c in line.chars() {
            match c {
                '>' => {
                    depth += 1;
                    spaces = 0;
                }
                ' ' => spaces += 1,
                '\t' => spaces += 4,
                _ => break,
            }
            // Indentation nests too, four spaces to a level.
            if spaces >= 4 {
                depth += 1;
                spaces = 0;
            }
            if depth > MAX_CONTAINER_DEPTH {
                return depth;
            }
        }
        worst = worst.max(depth);
    }
    worst
}

fn strip_images_and_html(src: &str) -> (String, MarkdownReport) {
    let Ok(tree) = to_mdast(src, &ParseOptions::gfm()) else {
        return (src.to_string(), MarkdownReport::default());
    };
    let mut report = MarkdownReport::default();
    let mut edits: Vec<Edit> = Vec::new();

    for node in walk(&tree) {
        match node {
            Node::Image(image) => {
                if let Some(span) = span(node) {
                    report.images_removed += 1;
                    edits.push(Edit {
                        span,
                        text: escape(&image.alt),
                    });
                }
            }
            Node::ImageReference(image) => {
                if let Some(span) = span(node) {
                    report.images_removed += 1;
                    edits.push(Edit {
                        span,
                        text: escape(&image.alt),
                    });
                }
            }
            Node::Html(_) => {
                if let Some(span) = span(node) {
                    report.html_removed += 1;
                    edits.push(Edit {
                        span,
                        text: String::new(),
                    });
                }
            }
            _ => {}
        }
    }

    (apply(src, edits), report)
}

fn defang_links(src: &str) -> (String, usize) {
    let Ok(tree) = to_mdast(src, &ParseOptions::gfm()) else {
        return (src.to_string(), 0);
    };
    let mut defanged = 0;
    let mut edits: Vec<Edit> = Vec::new();

    for node in walk(&tree) {
        let url = match node {
            Node::Link(link) => &link.url,
            // A reference-style link resolves through a Definition, whose URL
            // this pass cannot see from the reference alone. Definitions are
            // rewritten below instead, which covers every reference to them.
            _ => continue,
        };
        if scheme_allowed(url) {
            continue;
        }
        if let Some(span) = span(node) {
            defanged += 1;
            edits.push(Edit {
                span,
                text: escape(&visible_text(node)),
            });
        }
    }

    // Reference definitions carry the URL for `[text][id]` forms. Rewriting the
    // definition's target neutralizes every reference to it at once, without
    // having to resolve identifiers here.
    for node in walk(&tree) {
        let Node::Definition(def) = node else {
            continue;
        };
        if scheme_allowed(&def.url) {
            continue;
        }
        if let Some(span) = span(node) {
            defanged += 1;
            edits.push(Edit {
                span,
                text: String::new(),
            });
        }
    }

    (apply(src, edits), defanged)
}

/// True when `url` carries no scheme, or one on the allow list.
fn scheme_allowed(url: &str) -> bool {
    let Some(scheme) = scheme_of(url) else {
        return true;
    };
    ALLOWED_SCHEMES
        .iter()
        .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
}

/// The scheme of `url`, or `None` when it has none.
///
/// A `:` only introduces a scheme if it comes before any `/`, `?` or `#` —
/// otherwise `./a:b/c` and `#a:b` would look scheme-ful. Leading whitespace and
/// control characters are stripped first, because `java\tscript:` and
/// ` javascript:` are the classic ways past a naive prefix check.
fn scheme_of(url: &str) -> Option<&str> {
    let trimmed = url.trim_start_matches(|c: char| c.is_whitespace() || c.is_control());
    let colon = trimmed.find(':')?;
    let head = &trimmed[..colon];
    if head.is_empty() || head.contains(['/', '?', '#']) {
        return None;
    }
    // A real scheme is ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ). Anything
    // else is not a scheme, so treat it as a relative path.
    let mut chars = head.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some(head)
}

/// The text a node renders as, ignoring anything that carries a URL.
fn visible_text(node: &Node) -> String {
    let mut out = String::new();
    for descendant in walk(node) {
        match descendant {
            Node::Text(t) => out.push_str(&t.value),
            Node::InlineCode(c) => out.push_str(&c.value),
            _ => {}
        }
    }
    out
}

/// Escape the characters that could re-form markup once the replacement text is
/// spliced back into the source. Without this, an image whose alt text is
/// `x](https://evil)` would reassemble into a link.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '[' | ']' | '(' | ')' | '<' | '>' | '!' | '*' | '_' | '`' | '#' | '&'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

struct Edit {
    span: (usize, usize),
    text: String,
}

fn span(node: &Node) -> Option<(usize, usize)> {
    let position = node.position()?;
    Some((position.start.offset, position.end.offset))
}

/// Every node in the tree, parents before children.
///
/// An explicit stack, not recursion: a 512 KiB document of nested blockquotes
/// would overflow, and a stack overflow aborts the process and cannot be
/// caught. [`MAX_NODES`] bounds a pathological tree on top of that.
fn walk(root: &Node) -> Vec<&Node> {
    const MAX_NODES: usize = 200_000;

    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        out.push(node);
        if out.len() >= MAX_NODES {
            break;
        }
        if let Some(children) = node.children() {
            stack.extend(children.iter());
        }
    }
    out
}

/// Splice `edits` into `src`, outermost-wins and right-to-left.
///
/// Right-to-left so earlier offsets stay valid as later ones are replaced.
/// Outermost-wins because a defanged link can contain an edit of its own, and
/// applying both would corrupt the span.
fn apply(src: &str, mut edits: Vec<Edit>) -> String {
    if edits.is_empty() {
        return src.to_string();
    }
    // Outermost first: earliest start, then longest.
    edits.sort_by(|a, b| a.span.0.cmp(&b.span.0).then(b.span.1.cmp(&a.span.1)));

    let mut kept: Vec<Edit> = Vec::with_capacity(edits.len());
    let mut covered_to = 0usize;
    for edit in edits {
        if edit.span.0 < covered_to {
            continue;
        }
        covered_to = edit.span.1;
        kept.push(edit);
    }

    let mut out = src.to_string();
    for edit in kept.into_iter().rev() {
        let (start, end) = edit.span;
        // Offsets come from the parser, but splicing on a non-boundary would
        // panic, and this crate does not panic.
        if start > end
            || end > out.len()
            || !out.is_char_boundary(start)
            || !out.is_char_boundary(end)
        {
            continue;
        }
        out.replace_range(start..end, &edit.text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Neutralize, asserting the document was not refused outright.
    fn safe(src: &str) -> (String, MarkdownReport) {
        match neutralize(src) {
            Neutralized::Safe(text, report) => (text, report),
            Neutralized::RenderAsPlainText(reason) => {
                panic!("expected a renderable document, got refusal: {reason}")
            }
        }
    }

    /// Does the rendered tree still contain a node that carries a URL?
    fn has_url_sink(src: &str) -> bool {
        let Ok(tree) = to_mdast(src, &ParseOptions::gfm()) else {
            // Unparseable input renders as plain text; no sinks either way.
            return false;
        };
        walk(&tree).into_iter().any(|node| {
            matches!(
                node,
                Node::Image(_) | Node::ImageReference(_) | Node::Html(_)
            )
        })
    }

    fn surviving_link_urls(src: &str) -> Vec<String> {
        let Ok(tree) = to_mdast(src, &ParseOptions::gfm()) else {
            return Vec::new();
        };
        walk(&tree)
            .into_iter()
            .filter_map(|node| match node {
                Node::Link(link) => Some(link.url.clone()),
                Node::Definition(def) => Some(def.url.clone()),
                _ => None,
            })
            .collect()
    }

    /// The load-bearing test. Stated against the renderer's own parser, so it
    /// cannot drift from what gpui-component will actually build.
    #[test]
    fn a_neutralized_document_parses_to_a_tree_with_no_image_or_html_nodes() {
        let hostile = [
            "![beacon](https://attacker.example/px.png)",
            "![](http://a/b.png)",
            "<img src=\"https://attacker.example/px.png\">",
            "<IMG SRC='https://attacker.example/px.png'>",
            "text <img\n  src=\"https://a/b.png\"\n> more",
            "![ref][id]\n\n[id]: https://attacker.example/px.png",
            "[![nested](https://a/img.png)](https://b/click)",
            "<!-- comment --><img src=https://a/b.png>",
            "> quoted ![x](https://a/b.png)",
            "- item ![x](https://a/b.png)\n- other",
            "| a | b |\n|---|---|\n| ![x](https://a/b.png) | y |",
            "<div><span><img src='https://a/b.png'/></span></div>",
            "![a](https://a/1.png)![b](https://b/2.png)![c](https://c/3.png)",
        ];
        for case in hostile {
            let (clean, report) = safe(case);
            assert!(
                !has_url_sink(&clean),
                "a URL sink survived\n  input:  {case:?}\n  output: {clean:?}\n  report: {report:?}"
            );
        }
    }

    #[test]
    fn a_remote_markdown_image_is_replaced_by_its_alt_text() {
        let (clean, report) = safe("before ![the alt](https://a/b.png) after");
        assert_eq!(report.images_removed, 1);
        assert!(clean.contains("the alt"), "{clean:?}");
        assert!(!clean.contains("https://a/b.png"), "{clean:?}");
    }

    /// Local images take the identical code path to remote ones — gpui makes
    /// every markdown image a URI — so they are removed too.
    #[test]
    fn a_local_markdown_image_is_removed_as_well() {
        let (clean, report) = safe("![logo](./logo.png)");
        assert_eq!(report.images_removed, 1);
        assert!(!clean.contains("logo.png"), "{clean:?}");
    }

    #[test]
    fn a_reference_style_image_is_replaced_by_its_alt_text() {
        let (clean, report) = safe("![alt text][id]\n\n[id]: https://a/b.png");
        assert_eq!(report.images_removed, 1);
        assert!(!has_url_sink(&clean), "{clean:?}");
    }

    #[test]
    fn a_raw_html_img_tag_is_removed() {
        let (clean, report) = safe("<img src=\"https://a/b.png\">");
        assert_eq!(report.html_removed, 1);
        assert!(!clean.contains("https://a/b.png"), "{clean:?}");
    }

    /// A tag broken across lines never becomes an `Html` node at all: the
    /// parser leaves `<img` as literal text and GFM's autolink turns the bare
    /// URL into an ordinary `Link`. Nothing fetches it, and a user click opens
    /// their browser exactly as it would for any other https link — so the URL
    /// surviving *as visible text* is correct, not a leak.
    ///
    /// This test asserted `!clean.contains(url)` first and failed, which is how
    /// the real behaviour got checked instead of assumed.
    #[test]
    fn an_html_img_tag_split_across_lines_produces_no_image_node() {
        let src = "<img\n  src=\"https://a/b.png\"\n>";
        let (clean, _) = safe(src);
        assert!(!has_url_sink(&clean), "{clean:?}");
        assert_eq!(
            surviving_link_urls(&clean),
            vec!["https://a/b.png".to_string()],
            "the autolink should survive as a plain link"
        );
    }

    /// Documentation frequently *shows* markdown image syntax. Rewriting it
    /// inside a code block would visibly change real files, which the "same UI"
    /// requirement forbids.
    #[test]
    fn an_image_inside_a_fenced_code_block_is_left_alone() {
        let src = "```md\n![x](https://a/b.png)\n```";
        let (clean, report) = safe(src);
        assert_eq!(clean, src, "a code block was rewritten");
        assert!(report.is_empty());
    }

    #[test]
    fn an_image_inside_an_inline_code_span_is_left_alone() {
        let src = "use `![x](https://a/b.png)` for images";
        let (clean, report) = safe(src);
        assert_eq!(clean, src);
        assert!(report.is_empty());
    }

    #[test]
    fn a_link_to_a_shell_dispatchable_scheme_is_replaced_by_its_text() {
        for case in [
            "[click me](file:///etc/passwd)",
            "[click me](smb://host/share)",
            "[click me](javascript:alert(1))",
            "[click me](JaVaScRiPt:alert(1))",
            "[click me](  javascript:alert(1))",
        ] {
            let (clean, report) = safe(case);
            assert_eq!(report.links_defanged, 1, "{case:?} -> {clean:?}");
            assert!(clean.contains("click me"), "{case:?} -> {clean:?}");
            assert!(
                surviving_link_urls(&clean).is_empty(),
                "{case:?} -> {clean:?}"
            );
        }
    }

    #[test]
    fn ordinary_links_survive_untouched() {
        for case in [
            "[docs](https://example.com/a)",
            "[plain](http://example.com)",
            "[mail](mailto:someone@example.com)",
            "[anchor](#section)",
            "[relative](./other.md)",
        ] {
            let (clean, report) = safe(case);
            assert_eq!(clean, case, "an ordinary link was rewritten: {case:?}");
            assert_eq!(report.links_defanged, 0);
        }
    }

    /// An alt text carrying markup must not reassemble into a link once it is
    /// spliced back into the source.
    #[test]
    fn alt_text_cannot_reassemble_into_a_link() {
        let (clean, _) = safe(
            "![x](https://a/b.png)"
                .replace("x", "y](https://evil.example/px.png)![z")
                .as_str(),
        );
        assert!(!has_url_sink(&clean), "alt text reassembled: {clean:?}");
    }

    #[test]
    fn neutralizing_is_idempotent() {
        let src = "![a](https://a/b.png) and [c](file:///d) and <img src='https://e/f.png'>";
        let (once, _) = safe(src);
        let (twice, report) = safe(&once);
        assert_eq!(once, twice, "a second pass changed the document");
        assert!(
            report.is_empty(),
            "a second pass still found sinks: {report:?}"
        );
    }

    /// Regression guard for a real abort. This test first asserted that a
    /// 20 000-deep document neutralized fine; the process died with
    /// `fatal runtime error: stack overflow` instead. A stack overflow is not a
    /// panic — it aborts, and `catch_unwind` cannot see it — so the only
    /// mitigation is to refuse before parsing.
    ///
    /// This matters beyond this module: the renderer parses the same document
    /// with the same parser, so refusing here is what keeps the *renderer* from
    /// aborting on it too.
    #[test]
    fn a_pathologically_nested_document_is_refused_before_it_is_parsed() {
        let deep = "> ".repeat(20_000) + "![x](https://a/b.png)";
        assert!(
            matches!(neutralize(&deep), Neutralized::RenderAsPlainText(_)),
            "a document deep enough to abort the process was accepted"
        );
    }

    /// The cap must not catch documents people actually write.
    #[test]
    fn ordinarily_nested_documents_are_still_rendered() {
        let nested = "> ".repeat(8) + "quoted ![x](https://a/b.png)";
        let (clean, report) = safe(&nested);
        assert_eq!(report.images_removed, 1);
        assert!(!has_url_sink(&clean), "{clean:?}");

        // Deep-ish but legitimate: an eight-level indented list.
        let list = (0..8)
            .map(|i| format!("{}- item {i}", " ".repeat(i * 4)))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(matches!(neutralize(&list), Neutralized::Safe(..)));
    }

    /// Long runs of inline emphasis are handled iteratively by the parser, so
    /// they need no guard — asserted rather than assumed.
    #[test]
    fn long_inline_emphasis_runs_are_handled() {
        let stars = "*".repeat(10_000) + "text" + &"*".repeat(10_000);
        assert!(matches!(neutralize(&stars), Neutralized::Safe(..)));
    }

    #[test]
    fn the_depth_probe_agrees_with_the_documents_it_measures() {
        assert_eq!(container_depth("no nesting here"), 0);
        assert_eq!(container_depth("> one"), 1);
        assert_eq!(container_depth("> > > three"), 3);
        assert_eq!(container_depth("        eight spaces"), 2);
        assert!(container_depth(&"> ".repeat(20_000)) > MAX_CONTAINER_DEPTH);
    }

    #[test]
    fn a_document_with_no_sinks_is_returned_byte_for_byte() {
        let src = "# Title\n\nSome *text* with `code` and a [link](https://example.com).\n";
        let (clean, report) = safe(src);
        assert_eq!(clean, src);
        assert!(report.is_empty());
    }

    #[test]
    fn scheme_detection_does_not_mistake_a_path_for_a_scheme() {
        assert_eq!(scheme_of("https://a/b"), Some("https"));
        assert_eq!(scheme_of("mailto:a@b"), Some("mailto"));
        assert_eq!(scheme_of("./a:b/c"), None);
        assert_eq!(scheme_of("#a:b"), None);
        assert_eq!(scheme_of("/path/to:thing"), None);
        assert_eq!(scheme_of("no-colon"), None);
        assert_eq!(
            scheme_of("2bad:x"),
            None,
            "a scheme cannot start with a digit"
        );
        assert_eq!(scheme_of(" javascript:x"), Some("javascript"));
    }

    #[test]
    fn empty_and_whitespace_documents_are_handled() {
        assert_eq!(safe("").0, "");
        assert_eq!(safe("   \n\n  ").0, "   \n\n  ");
    }
}
