package main

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// writeDoc persists one page to the doc store as a `<hash>.doc` record in the
// format the Rust core reads (PLAN.md §7): RFC822-style headers, a blank line,
// then the raw body text. Each worker writes its own files, so there is no
// write contention.
func writeDoc(dir string, p Page, status int) error {
	sum := sha256.Sum256([]byte(p.URL))
	name := hex.EncodeToString(sum[:])[:16] + ".doc"
	path := filepath.Join(dir, name)

	// Header values must be single-line; collapse any stray newlines.
	var b strings.Builder
	fmt.Fprintf(&b, "url: %s\n", oneLine(p.URL))
	fmt.Fprintf(&b, "title: %s\n", oneLine(p.Title))
	fmt.Fprintf(&b, "status: %d\n", status)
	fmt.Fprintf(&b, "fetched: %s\n", time.Now().UTC().Format(time.RFC3339))
	if p.Published != "" {
		fmt.Fprintf(&b, "published: %s\n", oneLine(p.Published))
	}
	for _, img := range p.Images {
		if img.URL == "" {
			continue
		}
		fmt.Fprintf(&b, "image: %s\t%s\n", oneLine(img.URL), oneLine(img.Alt))
	}
	fmt.Fprintf(&b, "links: %s\n", strings.Join(p.Links, " "))
	b.WriteString("\n") // blank line separates headers from body
	b.WriteString(p.Text)
	b.WriteString("\n")

	// Atomic-ish write: temp file then rename, so a reader never sees a partial.
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, []byte(b.String()), 0o644); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

func oneLine(s string) string {
	return strings.TrimSpace(strings.NewReplacer("\n", " ", "\r", " ", "\t", " ").Replace(s))
}
