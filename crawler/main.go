// Omni crawler — Phase 2.
//
// A polite, concurrent web crawler that feeds the Omni index. It does a
// breadth-first crawl from seed URLs, stays within an allowlist of hosts
// (PLAN.md crawl scope: "curated set of sites"), respects robots.txt, rate-
// limits per host, and writes one plain-text record per page to the doc store
// (PLAN.md §7 storage decision) that the Rust core reads.
//
// Stdlib only — no external modules — so it builds offline and nothing hides
// the crawling logic, mirroring the from-scratch ethos of the Rust core.
//
// Usage:
//
//	go run . -seeds https://a.example,https://b.example \
//	         -out ../store -max 500 -workers 8 -delay 1s
//	go run . -seedfile seeds/academic.txt -out ../store -max 1500 -per-host 150
//
// Hosts to crawl are derived from the seeds unless -hosts is given. -per-host
// caps pages per domain so one big site can't dominate a multi-domain crawl.
package main

import (
	"bufio"
	"flag"
	"fmt"
	"os"
	"strings"
	"time"
)

type Config struct {
	Seeds     []string
	Hosts     map[string]bool // allowlist; empty entry means "derive from seeds"
	OutDir    string
	MaxPages  int
	PerHost   int // max pages per host (0 = unlimited)
	Workers   int
	Delay     time.Duration // default per-host politeness delay
	UserAgent string
	Timeout   time.Duration
}

func main() {
	var (
		seedsArg = flag.String("seeds", "", "comma-separated seed URLs")
		seedFile = flag.String("seedfile", "", "file of seed URLs, one per line ('#' comments)")
		hostsArg = flag.String("hosts", "", "comma-separated host allowlist (default: hosts of the seeds)")
		out      = flag.String("out", "../store", "doc-store output directory")
		maxPages = flag.Int("max", 200, "maximum pages to fetch (whole crawl)")
		perHost  = flag.Int("per-host", 0, "max pages per host (0 = unlimited)")
		workers  = flag.Int("workers", 8, "concurrent fetch workers")
		delay    = flag.Duration("delay", time.Second, "default per-host crawl delay")
		ua       = flag.String("ua", "OmniBot/0.1 (+https://github.com/your/omni)", "User-Agent")
		timeout  = flag.Duration("timeout", 15*time.Second, "per-request timeout")
	)
	flag.Parse()

	seeds := splitCSV(*seedsArg)
	if *seedFile != "" {
		fileSeeds, err := readSeedFile(*seedFile)
		if err != nil {
			fmt.Fprintf(os.Stderr, "crawler: cannot read seedfile %s: %v\n", *seedFile, err)
			os.Exit(1)
		}
		seeds = append(seeds, fileSeeds...)
	}
	if len(seeds) == 0 {
		fmt.Fprintln(os.Stderr, "crawler: need -seeds or -seedfile")
		flag.Usage()
		os.Exit(2)
	}

	cfg := Config{
		Seeds:     seeds,
		Hosts:     map[string]bool{},
		OutDir:    *out,
		MaxPages:  *maxPages,
		PerHost:   *perHost,
		Workers:   *workers,
		Delay:     *delay,
		UserAgent: *ua,
		Timeout:   *timeout,
	}
	for _, h := range splitCSV(*hostsArg) {
		cfg.Hosts[strings.ToLower(h)] = true
	}

	if err := os.MkdirAll(cfg.OutDir, 0o755); err != nil {
		fmt.Fprintf(os.Stderr, "crawler: cannot create out dir: %v\n", err)
		os.Exit(1)
	}

	c := NewCrawler(cfg)
	n := c.Run()
	fmt.Printf("crawler: done — %d page(s) written to %s\n", n, cfg.OutDir)
}

// readSeedFile reads seed URLs one per line, skipping blanks and '#' comments
// (inline comments after a URL are also trimmed).
func readSeedFile(path string) ([]string, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	var seeds []string
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		line := sc.Text()
		if i := strings.IndexByte(line, '#'); i >= 0 {
			line = line[:i]
		}
		if line = strings.TrimSpace(line); line != "" {
			seeds = append(seeds, line)
		}
	}
	return seeds, sc.Err()
}

func splitCSV(s string) []string {
	var out []string
	for _, p := range strings.Split(s, ",") {
		if p = strings.TrimSpace(p); p != "" {
			out = append(out, p)
		}
	}
	return out
}
