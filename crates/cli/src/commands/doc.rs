//! `audit doc` — serve the Basilisk documentation on localhost.
//!
//! All markdown pages are embedded at compile time via `include_str!`,
//! so the binary is fully self-contained. No docs directory needs to
//! exist at runtime.

use anyhow::{Context, Result};
use clap::Args;
use pulldown_cmark::{html, Options, Parser};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

// ---- embedded pages -------------------------------------------------------

struct Page {
    slug: &'static str,
    title: &'static str,
    markdown: &'static str,
}

static PAGES: &[Page] = &[
    Page {
        slug: "",
        title: "Overview",
        markdown: include_str!("../../../../docs/overview.md"),
    },
    Page {
        slug: "installation",
        title: "Installation",
        markdown: include_str!("../../../../docs/installation.md"),
    },
    Page {
        slug: "configuration",
        title: "Configuration",
        markdown: include_str!("../../../../docs/configuration.md"),
    },
    Page {
        slug: "commands",
        title: "Commands Reference",
        markdown: include_str!("../../../../docs/commands.md"),
    },
    Page {
        slug: "architecture",
        title: "Architecture",
        markdown: include_str!("../../../../docs/architecture.md"),
    },
    Page {
        slug: "agent",
        title: "Agent System",
        markdown: include_str!("../../../../docs/agent.md"),
    },
    Page {
        slug: "knowledge",
        title: "Knowledge Base",
        markdown: include_str!("../../../../docs/knowledge.md"),
    },
    Page {
        slug: "onchain",
        title: "On-Chain Analysis",
        markdown: include_str!("../../../../docs/onchain.md"),
    },
    Page {
        slug: "source",
        title: "Source Analysis",
        markdown: include_str!("../../../../docs/source.md"),
    },
    Page {
        slug: "vulns",
        title: "Vulnerability Analysis",
        markdown: include_str!("../../../../docs/vulns.md"),
    },
];

// ---- CLI args -------------------------------------------------------------

/// Serve the Basilisk documentation on localhost.
#[derive(Debug, Args)]
pub struct DocArgs {
    /// Port to listen on.
    #[arg(long, default_value = "3000")]
    port: u16,

    /// Open the documentation in the default browser immediately.
    #[arg(long)]
    open: bool,
}

// ---- entry point ----------------------------------------------------------

pub async fn run(args: &DocArgs) -> Result<()> {
    let addr = format!("127.0.0.1:{}", args.port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("cannot bind to port {} — is something already running there?", args.port))?;

    let url = format!("http://localhost:{}", args.port);
    println!("Basilisk documentation → {url}");
    println!("Press Ctrl-C to stop.\n");

    if args.open {
        open_browser(&url);
    }

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream).await {
                        tracing::debug!(error = %e, "doc connection dropped");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "doc server accept error");
            }
        }
    }
}

// ---- HTTP handling --------------------------------------------------------

async fn handle_connection(stream: tokio::net::TcpStream) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    // Read the request line ("GET /path HTTP/1.1")
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

    // Drain remaining headers so the TCP stream is left clean.
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).await?;
        if header == "\r\n" || header.is_empty() {
            break;
        }
    }

    let path = extract_path(&request_line);
    let response_bytes = build_response(path);
    writer.write_all(&response_bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Extract the URL path from "GET /foo HTTP/1.1" → "/foo".
fn extract_path(request_line: &str) -> &str {
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[1]
    } else {
        "/"
    }
}

/// Build a complete HTTP/1.1 response for the given path.
fn build_response(path: &str) -> Vec<u8> {
    // Strip leading slash and query string to get the slug.
    let slug = path
        .trim_start_matches('/')
        .split('?')
        .next()
        .unwrap_or("")
        .trim_end_matches('/');

    // Favicon — return minimal 404 without an HTML page.
    if slug == "favicon.ico" {
        return http_response(404, "text/plain", b"not found");
    }

    match PAGES.iter().find(|p| p.slug == slug) {
        Some(page) => {
            let body = render_page(page, slug);
            http_response(200, "text/html; charset=utf-8", body.as_bytes())
        }
        None => {
            let body = render_404(slug);
            http_response(404, "text/html; charset=utf-8", body.as_bytes())
        }
    }
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut out = header.into_bytes();
    out.extend_from_slice(body);
    out
}

// ---- rendering ------------------------------------------------------------

fn render_markdown(md: &str) -> String {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(md, opts);
    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);
    html_output
}

fn render_page(page: &Page, current_slug: &str) -> String {
    let content_html = render_markdown(page.markdown);
    let nav = build_nav(current_slug);
    page_shell(&page.title, &nav, &content_html)
}

fn render_404(slug: &str) -> String {
    let nav = build_nav("");
    let content = format!(
        "<h1>404 — Not Found</h1><p>No page at <code>/{slug}</code>.</p>\
         <p><a href=\"/\">← Back to Overview</a></p>"
    );
    page_shell("Not Found", &nav, &content)
}

fn build_nav(current_slug: &str) -> String {
    let mut nav = String::from(
        r#"<nav class="sidebar">
  <div class="sidebar-header">
    <div class="logo-mark">🐍</div>
    <h1>Basilisk</h1>
    <p>AI Smart-Contract Auditor</p>
  </div>
  <ul class="sidebar-nav">"#,
    );

    for page in PAGES {
        let href = if page.slug.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", page.slug)
        };
        let active = if page.slug == current_slug {
            r#" class="active""#
        } else {
            ""
        };
        nav.push_str(&format!(
            r#"<li><a href="{href}"{active}>{}</a></li>"#,
            page.title
        ));
    }

    nav.push_str("</ul></nav>");
    nav
}

fn page_shell(title: &str, nav: &str, content: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} — Basilisk Docs</title>
<style>{CSS}</style>
</head>
<body>
{nav}
<main class="content">
{content}
</main>
</body>
</html>"#,
        CSS = STYLE,
    )
}

// ---- browser opener -------------------------------------------------------

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();

    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();

    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .spawn();
}

// ---- embedded stylesheet --------------------------------------------------

const STYLE: &str = r#"
:root {
  --sidebar-bg: #1a1d24;
  --sidebar-fg: #9da5b4;
  --sidebar-active-fg: #61afef;
  --sidebar-active-bg: #21252f;
  --sidebar-hover-bg: #1e222a;
  --sidebar-width: 272px;
  --content-bg: #ffffff;
  --content-fg: #1c2024;
  --code-bg: #f6f8fa;
  --code-border: #d0d7de;
  --accent: #0969da;
  --accent-light: #dbeafe;
  --border: #d0d7de;
  --muted: #57606a;
  --heading-fg: #0d1117;
}
* { box-sizing: border-box; margin: 0; padding: 0; }
html, body { height: 100%; }
body {
  display: flex;
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
  font-size: 15px;
  line-height: 1.7;
  color: var(--content-fg);
  background: var(--content-bg);
}

/* ── Sidebar ── */
.sidebar {
  width: var(--sidebar-width);
  min-width: var(--sidebar-width);
  background: var(--sidebar-bg);
  color: var(--sidebar-fg);
  position: fixed;
  top: 0; left: 0; bottom: 0;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  border-right: 1px solid #13151b;
}
.sidebar-header {
  padding: 28px 20px 20px;
  border-bottom: 1px solid #23272f;
}
.logo-mark { font-size: 28px; margin-bottom: 6px; }
.sidebar-header h1 {
  font-size: 17px;
  font-weight: 700;
  color: #e8eaf0;
  letter-spacing: -0.3px;
}
.sidebar-header p {
  font-size: 11px;
  color: #555c6b;
  margin-top: 3px;
  text-transform: uppercase;
  letter-spacing: 0.4px;
}
.sidebar-nav {
  list-style: none;
  padding: 10px 0 20px;
  flex: 1;
}
.sidebar-nav li { }
.sidebar-nav a {
  display: block;
  padding: 7px 20px;
  color: var(--sidebar-fg);
  text-decoration: none;
  font-size: 13.5px;
  border-left: 3px solid transparent;
  transition: color 0.12s, background 0.12s;
}
.sidebar-nav a:hover {
  color: #c8cdd6;
  background: var(--sidebar-hover-bg);
}
.sidebar-nav a.active {
  color: var(--sidebar-active-fg);
  background: var(--sidebar-active-bg);
  border-left-color: var(--sidebar-active-fg);
  font-weight: 500;
}

/* ── Content area ── */
.content {
  margin-left: var(--sidebar-width);
  padding: 48px 64px 80px;
  max-width: calc(var(--sidebar-width) + 820px);
  min-height: 100vh;
}
.content > * + * { margin-top: 0; }

/* ── Typography ── */
h1 { font-size: 30px; color: var(--heading-fg); font-weight: 700;
     border-bottom: 1px solid var(--border); padding-bottom: 14px;
     margin-bottom: 24px; letter-spacing: -0.5px; }
h2 { font-size: 22px; color: var(--heading-fg); font-weight: 600;
     margin-top: 40px; margin-bottom: 14px; }
h3 { font-size: 17px; color: var(--heading-fg); font-weight: 600;
     margin-top: 28px; margin-bottom: 10px; }
h4 { font-size: 14px; color: var(--heading-fg); font-weight: 600;
     margin-top: 20px; margin-bottom: 8px; text-transform: uppercase;
     letter-spacing: 0.5px; }
p  { margin-bottom: 16px; }
ul, ol { margin-bottom: 16px; padding-left: 28px; }
li { margin-bottom: 5px; }
li > ul, li > ol { margin-bottom: 0; margin-top: 4px; }
a  { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
hr { border: none; border-top: 1px solid var(--border); margin: 32px 0; }
strong { font-weight: 600; }
em { font-style: italic; }
blockquote {
  border-left: 4px solid var(--sidebar-active-fg);
  background: var(--accent-light);
  padding: 14px 20px;
  margin-bottom: 20px;
  border-radius: 0 6px 6px 0;
  color: #1d4ed8;
}
blockquote p:last-child { margin-bottom: 0; }

/* ── Code ── */
code {
  font-family: "Fira Code", "Cascadia Code", "JetBrains Mono", ui-monospace, monospace;
  font-size: 0.875em;
  background: var(--code-bg);
  border: 1px solid var(--code-border);
  padding: 2px 6px;
  border-radius: 5px;
}
pre {
  background: #0d1117;
  border: 1px solid #30363d;
  border-radius: 8px;
  padding: 20px 24px;
  overflow-x: auto;
  margin-bottom: 20px;
  margin-top: 4px;
}
pre code {
  background: none;
  border: none;
  padding: 0;
  font-size: 13px;
  color: #e6edf3;
  line-height: 1.65;
}

/* ── Tables ── */
table {
  width: 100%;
  border-collapse: collapse;
  margin-bottom: 24px;
  font-size: 14px;
}
thead { background: var(--code-bg); }
th {
  font-weight: 600;
  text-align: left;
  padding: 10px 16px;
  border: 1px solid var(--border);
  color: var(--heading-fg);
}
td {
  padding: 9px 16px;
  border: 1px solid var(--border);
  vertical-align: top;
}
tbody tr:nth-child(even) td { background: #f9fafb; }

/* ── Responsive ── */
@media (max-width: 900px) {
  .sidebar { display: none; }
  .content { margin-left: 0; padding: 24px 20px; }
}
"#;
