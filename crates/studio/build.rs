//! Makes `cargo build` work without a Node toolchain.
//!
//! `rust-embed` needs its folder to exist when the macro expands. On a fresh
//! clone `web/dist/client` has not been built yet, so drop a placeholder page
//! there that explains how to produce the real one.

use std::fs;
use std::path::Path;

const PLACEHOLDER: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>minact studio</title>
<style>
  body { font: 14px ui-monospace, SFMono-Regular, Menlo, monospace;
         background: #0e1216; color: #e1e8ee; padding: 48px; line-height: 1.7; }
  code { background: #1b232b; padding: 2px 6px; border-radius: 3px; }
  a { color: #34b0c2; }
</style>
<h1>The Studio front-end has not been built</h1>
<p>This binary embeds a placeholder because <code>crates/studio/web/dist/client</code>
   was empty at compile time.</p>
<p>Build the front-end and recompile:</p>
<pre>cd crates/studio/web
npm install
npm run build
cargo build -p minact</pre>
<p>For front-end development run <code>npm run dev</code> instead — it proxies
   <code>/api</code> to this server on port 4000.</p>
"#;

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("web")
        .join("dist")
        .join("client");

    println!("cargo:rerun-if-changed={}", dist.display());

    let index = dist.join("index.html");
    if !index.exists() {
        fs::create_dir_all(&dist).expect("create studio asset directory");
        fs::write(&index, PLACEHOLDER).expect("write placeholder index.html");
    }
}
