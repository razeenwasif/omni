package main

import (
	"net/url"
	"regexp"
	"strconv"
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
	reScript     = regexp.MustCompile(`(?is)<script\b[^>]*>.*?</script>`)
	reStyle      = regexp.MustCompile(`(?is)<style\b[^>]*>.*?</style>`)
	reTitleBlock = regexp.MustCompile(`(?is)<title\b[^>]*>.*?</title>`)
	reNoscript   = regexp.MustCompile(`(?is)<noscript\b[^>]*>.*?</noscript>`)
	reTemplate   = regexp.MustCompile(`(?is)<template\b[^>]*>.*?</template>`)
	reSVG        = regexp.MustCompile(`(?is)<svg\b[^>]*>.*?</svg>`)
	reCanvas     = regexp.MustCompile(`(?is)<canvas\b[^>]*>.*?</canvas>`)
	reIframe     = regexp.MustCompile(`(?is)<iframe\b[^>]*>.*?</iframe>`)

	reTitle      = regexp.MustCompile(`(?is)<title\b[^>]*>(.*?)</title>`)
	reHref       = regexp.MustCompile(`(?is)<a\b[^>]*\bhref\s*=\s*("([^"]*)"|'([^']*)'|([^\s">]+))`)
	reTag        = regexp.MustCompile(`(?s)<[^>]*>`)
	reWhitespace = regexp.MustCompile(`\s+`)

	reMainTag       = regexp.MustCompile(`(?is)<main\b[^>]*>(.*?)</main>`)
	reArticleTag    = regexp.MustCompile(`(?is)<article\b[^>]*>(.*?)</article>`)
	reRoleMainTag   = regexp.MustCompile(`(?is)<(?:main|article|section|div)\b[^>]*\brole\s*=\s*["']main["'][^>]*>(.*?)</(?:main|article|section|div)>`)
	reContentAttr   = regexp.MustCompile(`(?is)<(?:main|article|section|div)\b[^>]*(?:id|class)\s*=\s*["'][^"']*(?:content|article|entry|post|docs?|markdown|prose|body)[^"']*["'][^>]*>(.*?)</(?:main|article|section|div)>`)
	reDropContainer = regexp.MustCompile(`(?is)<nav\b[^>]*>.*?</nav>|<footer\b[^>]*>.*?</footer>|<aside\b[^>]*>.*?</aside>|<form\b[^>]*>.*?</form>|<select\b[^>]*>.*?</select>|<button\b[^>]*>.*?</button>`)
	reNoiseAttr     = regexp.MustCompile(`(?is)<(?:nav|footer|aside|header|section|div|ul)\b[^>]*(?:id|class|role)\s*=\s*["'][^"']*(?:nav|navbar|menu|footer|sidebar|breadcrumb|cookie|consent|banner|advert|ads?|promo|subscribe|newsletter|modal|dialog|toolbar|pagination)[^"']*["'][^>]*>.*?</(?:nav|footer|aside|header|section|div|ul)>`)
	reBlockBoundary = regexp.MustCompile(`(?is)</?(?:p|div|section|article|main|header|h[1-6]|li|ul|ol|pre|code|blockquote|table|tr|br)\b[^>]*>`)

	// Publish-date signals, in order of reliability. A page rarely has all of
	// them; we take the first that matches.
	reMetaPubTag      = regexp.MustCompile(`(?is)<meta\b[^>]*\b(?:property|name|itemprop)\s*=\s*["'](?:article:published_time|datePublished|date|dc\.date|dc\.date\.issued|pubdate|sailthru\.date)["'][^>]*>`)
	reMetaContentAttr = regexp.MustCompile(`(?is)\bcontent\s*=\s*("([^"]*)"|'([^']*)'|([^\s">]+))`)
	reJSONLDDate      = regexp.MustCompile(`(?is)"datePublished"\s*:\s*"([^"]+)"`)
	reTimeAttr        = regexp.MustCompile(`(?is)<time\b[^>]*\bdatetime\s*=\s*["']([^"']+)["']`)
)

// extract pulls the title, readable text, publish date, and absolute outlinks
// from raw HTML. The text pass is still dependency-free, but now prefers main
// content and strips common boilerplate before the page is indexed.
func extract(pageURL, html string) Page {
	base, _ := url.Parse(pageURL)

	title := ""
	if m := reTitle.FindStringSubmatch(html); m != nil {
		title = collapse(stripTags(m[1]))
	}

	links := extractLinks(html, base)

	text := readableText(html)

	return Page{URL: pageURL, Title: title, Text: text, Published: extractPublished(html), Links: links}
}

// readableText extracts the most likely main content, strips obvious boilerplate,
// and keeps headings/code/body text in reading order. It is intentionally
// dependency-free; the crawler remains stdlib-only while avoiding the worst
// nav/footer/sidebar/cookie noise that hurts ranking and passage embeddings.
func readableText(raw string) string {
	clean := dropNonContent(raw)
	best := bestContentCandidate(clean)
	return collapse(stripTags(withBlockBoundaries(dropBoilerplate(best))))
}

func dropNonContent(s string) string {
	for _, re := range []*regexp.Regexp{
		reScript, reStyle, reTitleBlock, reNoscript, reTemplate, reSVG, reCanvas, reIframe,
	} {
		s = re.ReplaceAllString(s, " ")
	}
	return s
}

func dropBoilerplate(s string) string {
	// Run attr-based removal first, then semantic containers. Repeating once more
	// catches common nested menu/footer wrappers without pretending to be a DOM.
	for i := 0; i < 2; i++ {
		s = reNoiseAttr.ReplaceAllString(s, " ")
		s = reDropContainer.ReplaceAllString(s, " ")
	}
	return s
}

func bestContentCandidate(s string) string {
	type cand struct {
		html  string
		score int
	}
	var candidates []cand
	add := func(fragment string) {
		if text := collapse(stripTags(dropBoilerplate(fragment))); len(text) >= 80 {
			candidates = append(candidates, cand{html: fragment, score: contentScore(fragment, text)})
		}
	}

	for _, m := range reMainTag.FindAllStringSubmatch(s, -1) {
		add(m[1])
	}
	for _, m := range reArticleTag.FindAllStringSubmatch(s, -1) {
		add(m[1])
	}
	for _, m := range reRoleMainTag.FindAllStringSubmatch(s, -1) {
		add(m[1])
	}
	for _, m := range reContentAttr.FindAllStringSubmatch(s, -1) {
		add(m[1])
	}

	if len(candidates) == 0 {
		return s
	}
	best := candidates[0]
	for _, c := range candidates[1:] {
		if c.score > best.score {
			best = c
		}
	}
	return best.html
}

func contentScore(fragment, text string) int {
	words := len(strings.Fields(text))
	links := len(reHref.FindAllString(fragment, -1))
	lower := strings.ToLower(fragment)
	headings := strings.Count(lower, "<h1") + strings.Count(lower, "<h2") + strings.Count(lower, "<h3")
	code := strings.Count(lower, "<pre") + strings.Count(lower, "<code")
	return words + headings*12 + code*8 - links*6
}

func withBlockBoundaries(s string) string {
	return reBlockBoundary.ReplaceAllString(s, " ")
}

// extractPublished pulls a publish date from the page, trying the most reliable
// signals first: a `<meta>` publish tag, then JSON-LD `datePublished`, then a
// `<time datetime>`. Returns "" if none is present (most reference pages). The
// raw value is kept as-is; the Rust core parses the leading date.
func extractPublished(html string) string {
	if tag := reMetaPubTag.FindString(html); tag != "" {
		if m := reMetaContentAttr.FindStringSubmatch(tag); m != nil {
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

// unescapeEntities decodes common HTML entities before text is indexed. It
// handles named, decimal numeric, and hex numeric entities without depending on
// a non-stdlib parser.
func unescapeEntities(s string) string {
	if !strings.Contains(s, "&") {
		return s
	}
	var out strings.Builder
	out.Grow(len(s))
	for len(s) > 0 {
		i := strings.IndexByte(s, '&')
		if i < 0 {
			out.WriteString(s)
			break
		}
		out.WriteString(s[:i])
		s = s[i+1:]
		j := strings.IndexByte(s, ';')
		if j < 0 || j > 32 {
			out.WriteByte('&')
			continue
		}
		if ch, ok := entity(s[:j]); ok {
			out.WriteRune(ch)
			s = s[j+1:]
		} else {
			out.WriteByte('&')
		}
	}
	return out.String()
}

func entity(ent string) (rune, bool) {
	if strings.HasPrefix(ent, "#x") || strings.HasPrefix(ent, "#X") {
		n, err := strconv.ParseInt(ent[2:], 16, 32)
		if err == nil {
			return rune(n), true
		}
		return 0, false
	}
	if strings.HasPrefix(ent, "#") {
		n, err := strconv.ParseInt(ent[1:], 10, 32)
		if err == nil {
			return rune(n), true
		}
		return 0, false
	}
	switch ent {
	case "amp":
		return '&', true
	case "lt":
		return '<', true
	case "gt":
		return '>', true
	case "quot":
		return '"', true
	case "apos":
		return '\'', true
	case "nbsp":
		return ' ', true
	case "rsquo":
		return '\u2019', true
	case "lsquo":
		return '\u2018', true
	case "rdquo":
		return '\u201D', true
	case "ldquo":
		return '\u201C', true
	case "mdash":
		return '\u2014', true
	case "ndash":
		return '\u2013', true
	case "hellip":
		return '\u2026', true
	case "copy":
		return '\u00A9', true
	case "reg":
		return '\u00AE', true
	case "trade":
		return '\u2122', true
	case "deg":
		return '\u00B0', true
	default:
		return 0, false
	}
}

func firstNonEmpty(vals ...string) string {
	for _, v := range vals {
		if v != "" {
			return v
		}
	}
	return ""
}
