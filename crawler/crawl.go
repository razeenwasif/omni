package main

import (
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"time"
)

// Crawler holds the shared state for one crawl run.
type Crawler struct {
	cfg    Config
	client *http.Client

	mu        sync.Mutex
	visited   map[string]bool // normalized URLs already enqueued
	hostCount map[string]int  // pages enqueued per host (for -per-host cap)
	written   int             // pages actually saved
	stopped   bool            // hit MaxPages — stop enqueuing/fetching

	hostGate *hostGate // per-host rate limiting
	robots   *robotsCache

	queue chan string
	wg    sync.WaitGroup // counts outstanding URLs (enqueued but not finished)
}

func NewCrawler(cfg Config) *Crawler {
	return &Crawler{
		cfg:       cfg,
		client:    &http.Client{Timeout: cfg.Timeout},
		visited:   map[string]bool{},
		hostCount: map[string]int{},
		hostGate:  newHostGate(cfg.Delay),
		robots:    newRobotsCache(cfg.UserAgent),
		// Generously buffered so workers never block when enqueuing links;
		// total enqueues are bounded by MaxPages via the visited/stopped guards.
		queue: make(chan string, 1<<16),
	}
}

// Run performs the crawl and returns the number of pages written.
func (c *Crawler) Run() int {
	// Allowlist defaults to the hosts of the seeds.
	if len(c.cfg.Hosts) == 0 {
		for _, s := range c.cfg.Seeds {
			if u, err := url.Parse(s); err == nil {
				c.cfg.Hosts[strings.ToLower(u.Hostname())] = true
			}
		}
	}
	fmt.Printf("crawler: %d host(s) max=%d per-host=%d workers=%d delay=%s\n",
		len(c.cfg.Hosts), c.cfg.MaxPages, c.cfg.PerHost, c.cfg.Workers, c.cfg.Delay)

	for _, s := range c.cfg.Seeds {
		c.enqueue(s)
	}

	// Worker pool.
	var workers sync.WaitGroup
	for i := 0; i < c.cfg.Workers; i++ {
		workers.Add(1)
		go func() {
			defer workers.Done()
			for raw := range c.queue {
				c.process(raw)
				c.wg.Done()
			}
		}()
	}

	// Close the queue once every enqueued URL has been processed.
	go func() {
		c.wg.Wait()
		close(c.queue)
	}()

	workers.Wait()
	return c.written
}

// enqueue adds a URL if it's in scope, not yet seen, and we're under the cap.
func (c *Crawler) enqueue(raw string) {
	norm, ok := normalize(raw)
	if !ok {
		return
	}
	u, err := url.Parse(norm)
	if err != nil {
		return
	}
	host := strings.ToLower(u.Hostname())
	if !c.cfg.Hosts[host] {
		return // out of the curated scope
	}

	c.mu.Lock()
	if c.stopped || c.visited[norm] || len(c.visited) >= c.cfg.MaxPages {
		if len(c.visited) >= c.cfg.MaxPages {
			c.stopped = true
		}
		c.mu.Unlock()
		return
	}
	// Per-host cap: don't let one domain dominate a multi-domain crawl.
	if c.cfg.PerHost > 0 && c.hostCount[host] >= c.cfg.PerHost {
		c.mu.Unlock()
		return
	}
	c.visited[norm] = true
	c.hostCount[host]++
	c.mu.Unlock()

	c.wg.Add(1)
	c.queue <- norm
}

// process fetches one URL, stores it, and enqueues its in-scope links.
func (c *Crawler) process(raw string) {
	u, err := url.Parse(raw)
	if err != nil {
		return
	}

	// robots.txt gate.
	if !c.robots.allowed(c.client, u) {
		fmt.Printf("  skip (robots): %s\n", raw)
		return
	}

	// Politeness: wait out the per-host delay (honoring any Crawl-delay).
	delay := c.cfg.Delay
	if cd := c.robots.crawlDelay(u.Hostname()); cd > 0 {
		delay = cd
	}
	c.hostGate.wait(u.Hostname(), delay)

	body, ctype, status, err := c.fetch(raw)
	if err != nil {
		fmt.Printf("  err %s: %v\n", raw, err)
		return
	}
	if status != http.StatusOK || !strings.Contains(ctype, "text/html") {
		return
	}

	page := extract(raw, body)
	if err := writeDoc(c.cfg.OutDir, page, status); err != nil {
		fmt.Printf("  write err %s: %v\n", raw, err)
		return
	}

	c.mu.Lock()
	c.written++
	n := c.written
	c.mu.Unlock()
	fmt.Printf("  [%d] %s (%d links)\n", n, raw, len(page.Links))

	for _, link := range page.Links {
		c.enqueue(link)
	}
}

func (c *Crawler) fetch(raw string) (body string, contentType string, status int, err error) {
	req, err := http.NewRequest("GET", raw, nil)
	if err != nil {
		return "", "", 0, err
	}
	req.Header.Set("User-Agent", c.cfg.UserAgent)
	resp, err := c.client.Do(req)
	if err != nil {
		return "", "", 0, err
	}
	defer resp.Body.Close()

	// Cap body size to avoid pathological pages (2 MiB).
	limited := io.LimitReader(resp.Body, 2<<20)
	b, err := io.ReadAll(limited)
	if err != nil {
		return "", "", resp.StatusCode, err
	}
	return string(b), resp.Header.Get("Content-Type"), resp.StatusCode, nil
}

// normalize canonicalizes a URL for dedup: lowercases host, drops the fragment,
// and rejects non-http(s) schemes. Returns ok=false if unusable.
func normalize(raw string) (string, bool) {
	u, err := url.Parse(strings.TrimSpace(raw))
	if err != nil {
		return "", false
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return "", false
	}
	u.Fragment = ""
	u.Host = strings.ToLower(u.Host)
	if u.Path == "" {
		u.Path = "/"
	}
	return u.String(), true
}

// hostGate enforces a minimum delay between requests to the same host.
type hostGate struct {
	mu     sync.Mutex
	next   map[string]time.Time
	dfault time.Duration
}

func newHostGate(d time.Duration) *hostGate {
	return &hostGate{next: map[string]time.Time{}, dfault: d}
}

// wait blocks until it's polite to hit host again, then reserves the next slot.
func (g *hostGate) wait(host string, delay time.Duration) {
	if delay <= 0 {
		delay = g.dfault
	}
	g.mu.Lock()
	now := time.Now()
	earliest := g.next[host]
	if earliest.Before(now) {
		earliest = now
	}
	g.next[host] = earliest.Add(delay)
	g.mu.Unlock()

	if d := time.Until(earliest); d > 0 {
		time.Sleep(d)
	}
}
