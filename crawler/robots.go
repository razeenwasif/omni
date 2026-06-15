package main

import (
	"bufio"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"time"
)

// robotsCache fetches and caches robots.txt rules per host. It implements a
// pragmatic subset of the Robots Exclusion Protocol: the most-specific matching
// User-agent group, its Disallow/Allow prefixes, and Crawl-delay. Good enough to
// be polite; not a full RFC 9309 implementation.
type robotsCache struct {
	ua string

	mu    sync.Mutex
	rules map[string]*hostRules // keyed by host
}

type hostRules struct {
	disallow   []string
	allow      []string
	crawlDelay time.Duration
}

func newRobotsCache(ua string) *robotsCache {
	return &robotsCache{ua: ua, rules: map[string]*hostRules{}}
}

func (rc *robotsCache) get(client *http.Client, u *url.URL) *hostRules {
	host := u.Hostname()

	rc.mu.Lock()
	if r, ok := rc.rules[host]; ok {
		rc.mu.Unlock()
		return r
	}
	rc.mu.Unlock()

	r := rc.fetch(client, u)

	rc.mu.Lock()
	rc.rules[host] = r
	rc.mu.Unlock()
	return r
}

func (rc *robotsCache) fetch(client *http.Client, u *url.URL) *hostRules {
	robotsURL := u.Scheme + "://" + u.Host + "/robots.txt"
	req, err := http.NewRequest("GET", robotsURL, nil)
	if err != nil {
		return &hostRules{} // no rules → allow all
	}
	req.Header.Set("User-Agent", rc.ua)
	resp, err := client.Do(req)
	if err != nil {
		return &hostRules{}
	}
	defer resp.Body.Close()
	// Missing/forbidden robots.txt conventionally means "allow all".
	if resp.StatusCode != http.StatusOK {
		return &hostRules{}
	}
	return parseRobots(resp.Body, rc.ua)
}

func (rc *robotsCache) allowed(client *http.Client, u *url.URL) bool {
	return rc.get(client, u).permits(u.Path)
}

func (rc *robotsCache) crawlDelay(host string) time.Duration {
	rc.mu.Lock()
	defer rc.mu.Unlock()
	if r, ok := rc.rules[host]; ok {
		return r.crawlDelay
	}
	return 0
}

// permits applies longest-match Allow/Disallow precedence to a path.
func (r *hostRules) permits(path string) bool {
	if path == "" {
		path = "/"
	}
	longestDisallow, longestAllow := -1, -1
	for _, d := range r.disallow {
		if d != "" && strings.HasPrefix(path, d) && len(d) > longestDisallow {
			longestDisallow = len(d)
		}
	}
	for _, a := range r.allow {
		if strings.HasPrefix(path, a) && len(a) > longestAllow {
			longestAllow = len(a)
		}
	}
	// Allow wins ties (more permissive), per common robots.txt convention.
	return longestAllow >= longestDisallow
}

// parseRobots reads robots.txt and returns the rules for the group that best
// matches our user-agent, falling back to the wildcard "*" group.
func parseRobots(body interface{ Read([]byte) (int, error) }, ua string) *hostRules {
	uaToken := strings.ToLower(strings.SplitN(ua, "/", 2)[0]) // "omnibot/0.1" → "omnibot"

	type group struct {
		agents []string
		rules  hostRules
	}
	var groups []group
	var cur *group
	expectingAgent := false

	scanner := bufio.NewScanner(body)
	for scanner.Scan() {
		line := scanner.Text()
		if i := strings.IndexByte(line, '#'); i >= 0 {
			line = line[:i]
		}
		line = strings.TrimSpace(line)
		if line == "" {
			continue
		}
		key, val, ok := splitField(line)
		if !ok {
			continue
		}
		switch key {
		case "user-agent":
			if !expectingAgent || cur == nil {
				groups = append(groups, group{})
				cur = &groups[len(groups)-1]
			}
			cur.agents = append(cur.agents, strings.ToLower(val))
			expectingAgent = true
		case "disallow":
			if cur != nil {
				cur.rules.disallow = append(cur.rules.disallow, val)
			}
			expectingAgent = false
		case "allow":
			if cur != nil {
				cur.rules.allow = append(cur.rules.allow, val)
			}
			expectingAgent = false
		case "crawl-delay":
			if cur != nil {
				if secs, err := strconv.ParseFloat(val, 64); err == nil {
					cur.rules.crawlDelay = time.Duration(secs * float64(time.Second))
				}
			}
			expectingAgent = false
		}
	}

	// Prefer a group naming our token; else the wildcard group.
	var wildcard *hostRules
	for i := range groups {
		for _, a := range groups[i].agents {
			if a == uaToken {
				return &groups[i].rules
			}
			if a == "*" {
				wildcard = &groups[i].rules
			}
		}
	}
	if wildcard != nil {
		return wildcard
	}
	return &hostRules{}
}

func splitField(line string) (key, val string, ok bool) {
	i := strings.IndexByte(line, ':')
	if i < 0 {
		return "", "", false
	}
	return strings.ToLower(strings.TrimSpace(line[:i])), strings.TrimSpace(line[i+1:]), true
}
