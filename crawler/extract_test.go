package main

import (
	"strings"
	"testing"
)

func TestExtractKeepsMetadataAndLinks(t *testing.T) {
	page := `<!doctype html>
<html>
  <head>
    <title>Rust Guide</title>
    <meta property="article:published_time" content="2026-06-18T10:00:00Z">
    <meta property="og:image" content="/social-card.png">
  </head>
  <body>
    <main>
      <h1>Rust Guide</h1>
      <p>This guide explains ownership, borrowing, lifetimes, and safe memory management.</p>
      <img src="/images/ownership.png" alt="Rust ownership diagram">
      <p>It has enough useful body text to be selected as readable main page content.</p>
      <a href="/chapter-2">Next chapter</a>
    </main>
  </body>
</html>`

	got := extract("https://example.com/book/", page)
	if got.Title != "Rust Guide" {
		t.Fatalf("title mismatch: %q", got.Title)
	}
	if got.Published != "2026-06-18T10:00:00Z" {
		t.Fatalf("published mismatch: %q", got.Published)
	}
	if len(got.Links) != 1 || got.Links[0] != "https://example.com/chapter-2" {
		t.Fatalf("links mismatch: %#v", got.Links)
	}
	if len(got.Images) != 2 {
		t.Fatalf("images mismatch: %#v", got.Images)
	}
	if got.Images[0].URL != "https://example.com/social-card.png" || got.Images[0].Alt != "Rust Guide" {
		t.Fatalf("open graph image mismatch: %#v", got.Images[0])
	}
	if got.Images[1].URL != "https://example.com/images/ownership.png" || got.Images[1].Alt != "Rust ownership diagram" {
		t.Fatalf("img extraction mismatch: %#v", got.Images[1])
	}
	if !strings.Contains(got.Text, "ownership, borrowing, lifetimes") {
		t.Fatalf("text mismatch: %q", got.Text)
	}
}

func TestReadableTextPrefersMainAndDropsBoilerplate(t *testing.T) {
	page := `<!doctype html>
<html>
  <head><title>Example</title></head>
  <body>
    <nav class="navbar">Products Pricing Login Docs</nav>
    <aside class="sidebar">Related links that should not be indexed</aside>
    <main>
      <h1>Ownership and Borrowing</h1>
      <p>Rust ownership explains who is responsible for values in memory.</p>
      <p>Borrowing lets code use references without taking ownership of the value.</p>
      <p>These rules keep programs memory safe without requiring a garbage collector.</p>
    </main>
    <footer>Copyright Contact Privacy Terms</footer>
  </body>
</html>`

	text := readableText(page)
	for _, want := range []string{
		"Ownership and Borrowing",
		"Rust ownership explains",
		"Borrowing lets code use references",
		"memory safe",
	} {
		if !strings.Contains(text, want) {
			t.Fatalf("expected extracted text to contain %q; got %q", want, text)
		}
	}
	for _, noise := range []string{"Products Pricing Login", "Related links", "Copyright Contact"} {
		if strings.Contains(text, noise) {
			t.Fatalf("expected boilerplate %q to be removed; got %q", noise, text)
		}
	}
}

func TestReadableTextKeepsCodeBlocksInsideArticle(t *testing.T) {
	page := `<article>
  <h1>Fetch API</h1>
  <p>The fetch function starts a request and returns a promise for the response.</p>
  <pre><code>const res = await fetch("/api/search?q=rust")</code></pre>
  <p>Applications can then inspect status codes, headers, and decoded JSON bodies.</p>
</article>`

	text := readableText(page)
	for _, want := range []string{
		"Fetch API",
		"returns a promise",
		`const res = await fetch("/api/search?q=rust")`,
		"decoded JSON bodies",
	} {
		if !strings.Contains(text, want) {
			t.Fatalf("expected extracted text to contain %q; got %q", want, text)
		}
	}
}

func TestUnescapeEntitiesHandlesNamedDecimalAndHex(t *testing.T) {
	got := collapse(`Rust &amp; Go &#39;search&#39; &#x1F50D; &nbsp; docs`)
	want := "Rust & Go 'search' 🔍 docs"
	if got != want {
		t.Fatalf("entity decode mismatch:\n got: %q\nwant: %q", got, want)
	}
}

func TestExtractImagesHandlesSrcsetLazyAttrsAndDedupes(t *testing.T) {
	page := `<html><body>
	  <img srcset="/small.png 1x, /large.png 2x" alt="Small diagram">
	  <img data-src="/lazy.png" title="Lazy loaded figure">
	  <img src="/small.png" alt="Duplicate">
	  <img src="data:image/png;base64,abc" alt="Inline">
	</body></html>`

	got := extract("https://example.com/docs/page", page)
	if len(got.Images) != 2 {
		t.Fatalf("images mismatch: %#v", got.Images)
	}
	if got.Images[0].URL != "https://example.com/small.png" || got.Images[0].Alt != "Small diagram" {
		t.Fatalf("srcset image mismatch: %#v", got.Images[0])
	}
	if got.Images[1].URL != "https://example.com/lazy.png" || got.Images[1].Alt != "Lazy loaded figure" {
		t.Fatalf("lazy image mismatch: %#v", got.Images[1])
	}
}
