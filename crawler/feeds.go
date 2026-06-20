package main

import (
	"encoding/xml"
	"fmt"
	"net/url"
	"strings"
	"time"
)

type FeedEntry struct {
	URL       string
	Published string
}

func (c *Crawler) discoverFeeds() int {
	seen := map[string]bool{}
	total := 0
	for _, raw := range c.cfg.FeedURLs {
		total += c.discoverFeed(raw, 0, seen)
	}
	return total
}

func (c *Crawler) discoverFeed(raw string, depth int, seen map[string]bool) int {
	if depth > 2 {
		return 0
	}
	norm, ok := normalize(raw)
	if !ok || seen[norm] {
		return 0
	}
	seen[norm] = true
	u, err := url.Parse(norm)
	if err != nil {
		return 0
	}
	if !c.cfg.Hosts[strings.ToLower(u.Hostname())] {
		return 0
	}
	if !c.robots.allowed(c.client, u) {
		fmt.Printf("  skip feed (robots): %s\n", norm)
		return 0
	}
	if cd := c.robots.crawlDelay(u.Hostname()); cd > 0 {
		c.hostGate.wait(u.Hostname(), cd)
	} else {
		c.hostGate.wait(u.Hostname(), c.cfg.Delay)
	}

	body, _, status, err := c.fetch(norm)
	if err != nil {
		fmt.Printf("  feed err %s: %v\n", norm, err)
		return 0
	}
	if status < 200 || status >= 300 {
		return 0
	}
	entries, nested := parseFeedXML(norm, body)
	for _, sitemap := range nested {
		c.discoverFeed(sitemap, depth+1, seen)
	}
	added := 0
	for _, entry := range entries {
		if entry.URL == "" {
			continue
		}
		c.rememberPublished(entry.URL, entry.Published)
		c.enqueue(entry.URL)
		added++
	}
	if added > 0 || len(nested) > 0 {
		fmt.Printf("  feed %s (%d urls, %d nested)\n", norm, added, len(nested))
	}
	return added
}

func parseFeedXML(baseURL, body string) ([]FeedEntry, []string) {
	dec := xml.NewDecoder(strings.NewReader(body))
	dec.Strict = false

	type item struct {
		kind      string
		url       string
		published string
	}
	var (
		stack      []string
		cur        *item
		curSitemap string
		entries    []FeedEntry
		nested     []string
	)

	for {
		tok, err := dec.Token()
		if err != nil {
			break
		}
		switch t := tok.(type) {
		case xml.StartElement:
			name := strings.ToLower(t.Name.Local)
			stack = append(stack, name)
			switch name {
			case "item", "entry", "url":
				cur = &item{kind: name}
			case "sitemap":
				curSitemap = ""
			case "link":
				if cur != nil && cur.kind == "entry" && cur.url == "" {
					if href := atomLinkHref(t); href != "" {
						cur.url = href
					}
				}
			}
		case xml.CharData:
			field := currentField(stack)
			text := strings.TrimSpace(string(t))
			if text == "" {
				continue
			}
			if cur != nil {
				switch field {
				case "link":
					if cur.kind == "item" {
						cur.url += text
					}
				case "loc":
					if cur.kind == "url" {
						cur.url += text
					}
				case "pubdate", "lastmod", "updated", "published", "date":
					cur.published += text
				}
			}
			if curSitemap != "" || parentField(stack) == "sitemap" {
				if field == "loc" {
					curSitemap += text
				}
			}
		case xml.EndElement:
			name := strings.ToLower(t.Name.Local)
			switch name {
			case "item", "entry", "url":
				if cur != nil && cur.kind == name {
					if u := resolveFeedURL(baseURL, cur.url); u != "" {
						entries = append(entries, FeedEntry{
							URL:       u,
							Published: normalizeFeedDate(cur.published),
						})
					}
					cur = nil
				}
			case "sitemap":
				if u := resolveFeedURL(baseURL, curSitemap); u != "" {
					nested = append(nested, u)
				}
				curSitemap = ""
			}
			if len(stack) > 0 {
				stack = stack[:len(stack)-1]
			}
		}
	}
	return dedupeEntries(entries), dedupeStrings(nested)
}

func atomLinkHref(el xml.StartElement) string {
	var href, rel string
	for _, attr := range el.Attr {
		switch strings.ToLower(attr.Name.Local) {
		case "href":
			href = strings.TrimSpace(attr.Value)
		case "rel":
			rel = strings.ToLower(strings.TrimSpace(attr.Value))
		}
	}
	if href == "" || (rel != "" && rel != "alternate") {
		return ""
	}
	return href
}

func currentField(stack []string) string {
	if len(stack) == 0 {
		return ""
	}
	return stack[len(stack)-1]
}

func parentField(stack []string) string {
	if len(stack) < 2 {
		return ""
	}
	return stack[len(stack)-2]
}

func resolveFeedURL(baseURL, raw string) string {
	raw = strings.TrimSpace(raw)
	if raw == "" {
		return ""
	}
	base, err := url.Parse(baseURL)
	if err != nil {
		return ""
	}
	ref, err := url.Parse(raw)
	if err != nil {
		return ""
	}
	abs := base.ResolveReference(ref)
	if abs.Scheme != "http" && abs.Scheme != "https" {
		return ""
	}
	abs.Fragment = ""
	return abs.String()
}

func normalizeFeedDate(raw string) string {
	raw = strings.TrimSpace(raw)
	if raw == "" {
		return ""
	}
	layouts := []string{
		time.RFC3339,
		time.RFC1123Z,
		time.RFC1123,
		time.RFC822Z,
		time.RFC822,
		"2006-01-02",
		"2006-01-02 15:04:05",
		"Mon, 2 Jan 2006 15:04:05 -0700",
		"Mon, 2 Jan 2006 15:04:05 MST",
	}
	for _, layout := range layouts {
		if t, err := time.Parse(layout, raw); err == nil {
			return t.UTC().Format(time.RFC3339)
		}
	}
	return raw
}

func dedupeEntries(entries []FeedEntry) []FeedEntry {
	seen := map[string]bool{}
	var out []FeedEntry
	for _, entry := range entries {
		if entry.URL == "" || seen[entry.URL] {
			continue
		}
		seen[entry.URL] = true
		out = append(out, entry)
	}
	return out
}

func dedupeStrings(items []string) []string {
	seen := map[string]bool{}
	var out []string
	for _, item := range items {
		if item == "" || seen[item] {
			continue
		}
		seen[item] = true
		out = append(out, item)
	}
	return out
}
