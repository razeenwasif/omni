package main

import (
	"net/url"
	"regexp"
	"strings"
)

// Page is the extracted content of one fetched HTML document.
type Page struct {
	URL       string
	Title     string
	Text      string
	Published string   // raw publish date if found (empty = unknown)
	Links     []string // absolute, in-scope-filtering happens at enqueue time
}

var (
	reScriptStyle = regexp.MustCompile(`(?is)<(script|style|title)\b[^>]*>.*?</(script|style|title)>`)
	reTitle       = regexp.MustCompile(`(?is)<title\b[^>]*>(.*?)</title>`)
	reHref        = regexp.MustCompile(`(?is)<a\b[^>]*\bhref\s*=\s*("([^"]*)"|'([^']*)'|([^\s">]+))`)
	reTag         = regexp.MustCompile(`(?s)<[^>]*>`)
	reWhitespace  = regexp.MustCompile(`\s+`)

	// Publish-date signals, in order of reliability. A page rarely has all of
	// them; we take the first that matches.
	reMetaPubTag  = regexp.MustCompile(`(?is)<meta\b[^>]*\b(?:property|name|itemprop)\s*=\s*["'](?:article:published_time|datePublished|date|dc\.date|dc\.date\.issued|pubdate|sailthru\.date)["'][^>]*>`)
	reContentAttr = regexp.MustCompile(`(?is)\bcontent\s*=\s*("([^"]*)"|'([^']*)'|([^\s">]+))`)
	reJSONLDDate  = regexp.MustCompile(`(?is)"datePublished"\s*:\s*"([^"]+)"`)
	reTimeAttr    = regexp.MustCompile(`(?is)<time\b[^>]*\bdatetime\s*=\s*["']([^"']+)["']`)
)

// extract pulls the title, visible text, and absolute outlinks from raw HTML.
// Stdlib regex-based extraction — crude but dependency-free; a real DOM parser
// is deferred (PLAN.md). Boilerplate removal lands in the Python pipeline later.
func extract(pageURL, html string) Page {
	base, _ := url.Parse(pageURL)

	title := ""
	if m := reTitle.FindStringSubmatch(html); m != nil {
		title = collapse(stripTags(m[1]))
	}

	links := extractLinks(html, base)

	// Visible body text = HTML minus script/style/title blocks minus all tags
	// (the title is indexed separately, so keep it out of the body).
	noBlocks := reScriptStyle.ReplaceAllString(html, " ")
	text := collapse(stripTags(noBlocks))

	return Page{URL: pageURL, Title: title, Text: text, Published: extractPublished(html), Links: links}
}

// extractPublished pulls a publish date from the page, trying the most reliable
// signals first: a `<meta>` publish tag, then JSON-LD `datePublished`, then a
// `<time datetime>`. Returns "" if none is present (most reference pages). The
// raw value is kept as-is; the Rust core parses the leading date.
func extractPublished(html string) string {
	if tag := reMetaPubTag.FindString(html); tag != "" {
		if m := reContentAttr.FindStringSubmatch(tag); m != nil {
			if v := strings.TrimSpace(firstNonEmpty(m[2], m[3], m[4])); v != "" {
				return v
			}
		}
	}
	if m := reJSONLDDate.FindStringSubmatch(html); m != nil {
		if v := strings.TrimSpace(m[1]); v != "" {
			return v
		}
	}
	if m := reTimeAttr.FindStringSubmatch(html); m != nil {
		if v := strings.TrimSpace(m[1]); v != "" {
			return v
		}
	}
	return ""
}

func extractLinks(html string, base *url.URL) []string {
	seen := map[string]bool{}
	var out []string
	for _, m := range reHref.FindAllStringSubmatch(html, -1) {
		href := firstNonEmpty(m[2], m[3], m[4])
		if href == "" || strings.HasPrefix(href, "#") {
			continue
		}
		ref, err := url.Parse(href)
		if err != nil {
			continue
		}
		abs := base.ResolveReference(ref)
		if abs.Scheme != "http" && abs.Scheme != "https" {
			continue
		}
		abs.Fragment = ""
		s := abs.String()
		if !seen[s] {
			seen[s] = true
			out = append(out, s)
		}
	}
	return out
}

func stripTags(s string) string { return reTag.ReplaceAllString(s, " ") }

func collapse(s string) string {
	return strings.TrimSpace(reWhitespace.ReplaceAllString(unescapeEntities(s), " "))
}

// unescapeEntities handles the handful of HTML entities common in visible text.
func unescapeEntities(s string) string {
	r := strings.NewReplacer(
		"&amp;", "&", "&lt;", "<", "&gt;", ">",
		"&quot;", "\"", "&#39;", "'", "&apos;", "'", "&nbsp;", " ",
	)
	return r.Replace(s)
}

func firstNonEmpty(vals ...string) string {
	for _, v := range vals {
		if v != "" {
			return v
		}
	}
	return ""
}
