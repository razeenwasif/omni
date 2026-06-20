package main

import "testing"

func TestParseFeedXMLReadsRSSItemsAndDates(t *testing.T) {
	body := `<rss><channel>
	  <item>
	    <title>Release</title>
	    <link>/news/release</link>
	    <pubDate>Mon, 02 Jan 2006 15:04:05 -0700</pubDate>
	  </item>
	  <item>
	    <link>https://example.com/news/other</link>
	    <pubDate>2026-06-20</pubDate>
	  </item>
	</channel></rss>`

	entries, nested := parseFeedXML("https://example.com/feed.xml", body)
	if len(nested) != 0 {
		t.Fatalf("unexpected nested sitemaps: %#v", nested)
	}
	if len(entries) != 2 {
		t.Fatalf("entries mismatch: %#v", entries)
	}
	if entries[0].URL != "https://example.com/news/release" {
		t.Fatalf("rss link mismatch: %#v", entries[0])
	}
	if entries[0].Published != "2006-01-02T22:04:05Z" {
		t.Fatalf("rss date mismatch: %#v", entries[0])
	}
	if entries[1].Published != "2026-06-20T00:00:00Z" {
		t.Fatalf("iso date mismatch: %#v", entries[1])
	}
}

func TestParseFeedXMLReadsAtomEntries(t *testing.T) {
	body := `<feed>
	  <entry>
	    <link rel="alternate" href="/posts/atom-entry"/>
	    <updated>2026-06-20T08:30:00+10:00</updated>
	  </entry>
	  <entry>
	    <link rel="self" href="/feed-entry-metadata"/>
	    <link href="/posts/default-link"/>
	  </entry>
	</feed>`

	entries, _ := parseFeedXML("https://example.com/atom.xml", body)
	if len(entries) != 2 {
		t.Fatalf("entries mismatch: %#v", entries)
	}
	if entries[0].URL != "https://example.com/posts/atom-entry" {
		t.Fatalf("atom alternate mismatch: %#v", entries[0])
	}
	if entries[0].Published != "2026-06-19T22:30:00Z" {
		t.Fatalf("atom date mismatch: %#v", entries[0])
	}
	if entries[1].URL != "https://example.com/posts/default-link" {
		t.Fatalf("atom default link mismatch: %#v", entries[1])
	}
}

func TestParseFeedXMLReadsSitemapsAndSitemapIndexes(t *testing.T) {
	sitemap := `<urlset>
	  <url><loc>https://example.com/a</loc><lastmod>2026-06-18</lastmod></url>
	  <url><loc>/b</loc><lastmod>2026-06-19</lastmod></url>
	  <url><loc>https://example.com/a</loc><lastmod>2026-06-18</lastmod></url>
	</urlset>`
	entries, nested := parseFeedXML("https://example.com/sitemap.xml", sitemap)
	if len(nested) != 0 {
		t.Fatalf("unexpected nested sitemaps: %#v", nested)
	}
	if len(entries) != 2 {
		t.Fatalf("deduped sitemap entries mismatch: %#v", entries)
	}
	if entries[1].URL != "https://example.com/b" || entries[1].Published != "2026-06-19T00:00:00Z" {
		t.Fatalf("sitemap entry mismatch: %#v", entries[1])
	}

	index := `<sitemapindex>
	  <sitemap><loc>/sitemap-news.xml</loc></sitemap>
	  <sitemap><loc>https://example.com/sitemap-docs.xml</loc></sitemap>
	  <sitemap><loc>/sitemap-news.xml</loc></sitemap>
	</sitemapindex>`
	entries, nested = parseFeedXML("https://example.com/sitemap.xml", index)
	if len(entries) != 0 {
		t.Fatalf("unexpected sitemap-index entries: %#v", entries)
	}
	if len(nested) != 2 || nested[0] != "https://example.com/sitemap-news.xml" {
		t.Fatalf("nested sitemap mismatch: %#v", nested)
	}
}
