package main

// Behavioural oracle for the rate limiter (channels/app/ratelimit.go) and the GCRA it is built on
// (github.com/throttled/throttled/v2 rate.go, store/memstore), written to
// fixtures/behaviour_ratelimit.json and asserted by `mm_api::ratelimit`'s `go_parity` module.
//
// Derived from the **AGPL** half of the tree, so it feeds `mm-api`'s tests only.
//
// # Three corpora
//
//   - `gcra`: throttled's own `GCRARateLimiterCtx` over its own `MemStore`, with the store's clock
//     replaced (`SetTimeNow`) so every step happens at a stated nanosecond. This is the
//     arithmetic — Remaining's integer division, ResetAfter and RetryAfter, the limit being
//     MaxBurst+1, the tolerance, a period that does not divide a second, and the LRU eviction
//     of `MemoryStoreSize` — without any wall-clock noise.
//   - `writer`: Mattermost's own `RateLimiter.RateLimitWriter` into an `httptest.ResponseRecorder`,
//     for bursts fired back to back. The headers it writes (`setRateLimitHeaders`: seconds rounded
//     **up**, `Retry-After` only when limited) and the 429 `http.Error` body are Go's code. The
//     clock here is real, but a burst takes microseconds and every value it writes is a ceiling
//     or a floor of a quantity whose exact value (at zero elapsed time) is an integer multiple, so
//     the output is deterministic — the Rust side replays each burst at a single instant.
//   - `generate_key`: `RateLimiter.GenerateKey` over requests read by `http.ReadRequest`, for every
//     combination of VaryByUser / VaryByRemoteAddr / VaryByHeader and each token location.

import (
	"bufio"
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"sort"
	"strings"
	"time"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/v8/channels/app"
	"github.com/throttled/throttled/v2"
	"github.com/throttled/throttled/v2/store/memstore"
)

type gcraStep struct {
	AtNs int64  `json:"at_ns"`
	Key  string `json:"key"`
}

type gcraCase struct {
	Name      string     `json:"name"`
	PerSec    int        `json:"per_sec"`
	MaxBurst  int        `json:"max_burst"`
	StoreSize int        `json:"store_size"`
	Steps     []gcraStep `json:"steps"`
}

func burst(n int, atNs int64, key string) []gcraStep {
	out := make([]gcraStep, 0, n)
	for i := 0; i < n; i++ {
		out = append(out, gcraStep{AtNs: atNs, Key: key})
	}
	return out
}

func steps(parts ...[]gcraStep) []gcraStep {
	var out []gcraStep
	for _, p := range parts {
		out = append(out, p...)
	}
	return out
}

func at(ns int64, key string) []gcraStep { return []gcraStep{{AtNs: ns, Key: key}} }

const ms = int64(time.Millisecond)
const sec = int64(time.Second)

var gcraCorpus = []gcraCase{
	// `POST /users/login`: PerSec 5, MaxBurst 10 (api4/user.go:69).
	{Name: "login burst then recovery", PerSec: 5, MaxBurst: 10, StoreSize: 10000, Steps: steps(
		burst(13, 0, "10.0.0.1"),
		at(199*ms, "10.0.0.1"),
		at(200*ms, "10.0.0.1"),
		at(201*ms, "10.0.0.1"),
		at(399*ms, "10.0.0.1"),
		at(600*ms, "10.0.0.1"),
		at(2400*ms, "10.0.0.1"),
		at(10*sec, "10.0.0.1"),
		at(10*sec, "10.0.0.1"),
	)},
	// `/users/login/desktop_token` and `/oauth/apps/register`: PerSec 2, MaxBurst 1.
	{Name: "desktop token", PerSec: 2, MaxBurst: 1, StoreSize: 10000, Steps: steps(
		burst(4, 0, "k"),
		at(499*ms, "k"),
		at(500*ms, "k"),
		at(500*ms, "k"),
		at(999*ms, "k"),
		at(1000*ms, "k"),
		at(1001*ms, "k"),
		at(5*sec, "k"),
	)},
	// The global default: PerSec 10, MaxBurst 100 (config.go SetDefaults).
	{Name: "global default", PerSec: 10, MaxBurst: 100, StoreSize: 10000, Steps: steps(
		burst(103, 0, "ip"),
		at(50*ms, "ip"),
		at(100*ms, "ip"),
		at(100*ms, "ip"),
	)},
	// MaxBurst 0: a limit of one.
	{Name: "no burst", PerSec: 1, MaxBurst: 0, StoreSize: 10000, Steps: steps(
		at(0, "a"),
		at(1, "a"),
		at(500*ms, "a"),
		at(999999999, "a"),
		at(sec, "a"),
		at(sec+1, "a"),
		at(3*sec, "a"),
	)},
	// A period that does not divide a second: time.Second / 3 truncates to 333333333ns.
	{Name: "period truncates", PerSec: 3, MaxBurst: 2, StoreSize: 10000, Steps: steps(
		burst(5, 0, "a"),
		at(333333332, "a"),
		at(333333333, "a"),
		at(333333334, "a"),
		at(700*ms, "a"),
		at(1500*ms, "a"),
	)},
	{Name: "period truncates by seven", PerSec: 7, MaxBurst: 3, StoreSize: 10000, Steps: steps(
		burst(6, 0, "a"),
		at(142857142, "a"),
		at(142857143, "a"),
		at(2*sec, "a"),
	)},
	// Keys are independent.
	{Name: "two keys", PerSec: 2, MaxBurst: 1, StoreSize: 10000, Steps: steps(
		burst(3, 0, "a"),
		burst(3, 0, "b"),
		at(100*ms, "a"),
		at(100*ms, ""),
		at(100*ms, ""),
		at(100*ms, ""),
	)},
	// `MemoryStoreSize` is an LRU of keys: a key pushed out forgets its history, and a read — even
	// a refused request's — refreshes a key's recency.
	{Name: "lru of two evicts the least recent", PerSec: 1, MaxBurst: 0, StoreSize: 2, Steps: steps(
		at(0, "a"),
		at(1, "a"),
		at(2, "b"),
		at(3, "c"),
		at(4, "a"),
		at(5, "b"),
		at(6, "c"),
	)},
	{Name: "lru of two keeps a key a refused request touched", PerSec: 1, MaxBurst: 0, StoreSize: 2, Steps: steps(
		at(0, "a"),
		at(1, "b"),
		at(2, "a"),
		at(3, "c"),
		at(4, "a"),
		at(5, "b"),
	)},
	{Name: "lru of one", PerSec: 1, MaxBurst: 0, StoreSize: 1, Steps: steps(
		at(0, "a"),
		at(1, "a"),
		at(2, "b"),
		at(3, "a"),
	)},
}

func runGCRACase(c gcraCase) ([]map[string]any, error) {
	st, err := memstore.New(c.StoreSize)
	if err != nil {
		return nil, err
	}
	base := time.Unix(1_790_000_000, 0)
	var now time.Time
	st.SetTimeNow(func() time.Time { return now })
	limiter, err := throttled.NewGCRARateLimiterCtx(throttled.WrapStoreWithContext(st), throttled.RateQuota{
		MaxRate:  throttled.PerSec(c.PerSec),
		MaxBurst: c.MaxBurst,
	})
	if err != nil {
		return nil, err
	}
	out := make([]map[string]any, 0, len(c.Steps))
	for _, s := range c.Steps {
		now = base.Add(time.Duration(s.AtNs))
		limited, res, err := limiter.RateLimitCtx(context.Background(), s.Key, 1)
		if err != nil {
			return nil, err
		}
		out = append(out, map[string]any{
			"at_ns":          s.AtNs,
			"key":            s.Key,
			"limited":        limited,
			"limit":          res.Limit,
			"remaining":      res.Remaining,
			"reset_ns":       res.ResetAfter.Nanoseconds(),
			"retry_after_ns": res.RetryAfter.Nanoseconds(),
		})
	}
	return out, nil
}

type writerCase struct {
	Name     string `json:"name"`
	PerSec   int    `json:"per_sec"`
	MaxBurst int    `json:"max_burst"`
	Requests int    `json:"requests"`
}

var writerCorpus = []writerCase{
	{Name: "login", PerSec: 5, MaxBurst: 10, Requests: 14},
	{Name: "desktop token and oauth register", PerSec: 2, MaxBurst: 1, Requests: 4},
	{Name: "global default", PerSec: 10, MaxBurst: 100, Requests: 103},
	{Name: "one per second", PerSec: 1, MaxBurst: 0, Requests: 3},
	{Name: "slow", PerSec: 1, MaxBurst: 4, Requests: 7},
}

func headerRows(h http.Header) [][2]string {
	keys := make([]string, 0, len(h))
	for k := range h {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	out := [][2]string{}
	for _, k := range keys {
		for _, v := range h[k] {
			out = append(out, [2]string{k, v})
		}
	}
	return out
}

func runWriterCase(c writerCase) ([]map[string]any, error) {
	settings := model.RateLimitSettings{PerSec: new(c.PerSec), MaxBurst: new(c.MaxBurst)}
	settings.SetDefaults()
	rl, err := app.NewRateLimiter(&settings, []string{})
	if err != nil {
		return nil, err
	}
	out := make([]map[string]any, 0, c.Requests)
	for i := 0; i < c.Requests; i++ {
		rec := httptest.NewRecorder()
		limited := rl.RateLimitWriter(context.Background(), "10.0.0.1", rec)
		status := 0
		if limited {
			status = rec.Code
		}
		out = append(out, map[string]any{
			"limited": limited,
			"status":  status,
			"headers": headerRows(rec.Header()),
			"body":    rec.Body.String(),
		})
	}
	return out, nil
}

type keyRequest struct {
	Name       string      `json:"name"`
	Headers    [][2]string `json:"headers"`
	Query      string      `json:"query"`
	RemoteAddr string      `json:"remote_addr"`
}

type keySettings struct {
	VaryByUser       bool     `json:"vary_by_user"`
	VaryByRemoteAddr bool     `json:"vary_by_remote_addr"`
	VaryByHeader     string   `json:"vary_by_header"`
	Trusted          []string `json:"trusted"`
}

var keyRequests = []keyRequest{
	{Name: "anonymous", RemoteAddr: "10.0.0.1:5000"},
	{Name: "anonymous v6", RemoteAddr: "[2001:db8::1]:5000"},
	{Name: "bearer", Headers: h("Authorization", "Bearer abcdefghijklmnopqrstuvwxyz"), RemoteAddr: "10.0.0.1:5000"},
	{Name: "oauth token header", Headers: h("Authorization", "token oauthtokenoauthtokenoauth"), RemoteAddr: "10.0.0.1:5000"},
	{Name: "cookie beats bearer", Headers: h("Cookie", "MMAUTHTOKEN=cookietoken", "Authorization", "Bearer headertoken"), RemoteAddr: "10.0.0.1:5000"},
	{Name: "empty cookie is a token", Headers: h("Cookie", "MMAUTHTOKEN="), RemoteAddr: "10.0.0.1:5000"},
	{Name: "query token", Query: "access_token=querytoken", RemoteAddr: "10.0.0.1:5000"},
	{Name: "cloud token", Headers: h("X-Cloud-Token", "cloudtoken"), RemoteAddr: "10.0.0.1:5000"},
	{Name: "remote cluster token", Headers: h("X-RemoteCluster-Token", "remotetoken"), RemoteAddr: "10.0.0.1:5000"},
	{Name: "long token is truncated to fifty", Headers: h("Authorization", "Bearer "+strings.Repeat("t", 60)), RemoteAddr: "10.0.0.1:5000"},
	{Name: "short bearer is no token", Headers: h("Authorization", "Bearer"), RemoteAddr: "10.0.0.1:5000"},
	{Name: "xff", Headers: h("X-Forwarded-For", "1.2.3.4, 5.6.7.8"), RemoteAddr: "10.0.0.1:5000"},
	{Name: "vary header upper case value", Headers: h("X-Client-Id", "MiXeD-Case"), RemoteAddr: "10.0.0.1:5000"},
	{Name: "vary header and bearer and xff", Headers: h("X-Client-Id", "ABC", "Authorization", "Bearer tok", "X-Forwarded-For", "9.9.9.9"), RemoteAddr: "10.0.0.1:5000"},
	{Name: "vary header repeated reads the first", Headers: h("X-Client-Id", "First", "X-Client-Id", "Second"), RemoteAddr: "10.0.0.1:5000"},
	{Name: "unix socket", RemoteAddr: "@"},
}

var keySettingsCorpus = []keySettings{
	{VaryByRemoteAddr: true},
	{VaryByRemoteAddr: false},
	{VaryByUser: true, VaryByRemoteAddr: true},
	{VaryByUser: true, VaryByRemoteAddr: false},
	{VaryByRemoteAddr: true, VaryByHeader: "X-Client-Id"},
	{VaryByRemoteAddr: false, VaryByHeader: "x-client-id"},
	{VaryByUser: true, VaryByRemoteAddr: true, VaryByHeader: "X-Client-Id"},
	{VaryByRemoteAddr: true, Trusted: []string{"X-Forwarded-For"}},
	{VaryByUser: true, VaryByRemoteAddr: true, Trusted: []string{"X-Forwarded-For"}},
}

func runKeyCorpus() ([]map[string]any, error) {
	out := []map[string]any{}
	for _, s := range keySettingsCorpus {
		settings := model.RateLimitSettings{
			VaryByUser:       new(s.VaryByUser),
			VaryByRemoteAddr: new(s.VaryByRemoteAddr),
			VaryByHeader:     s.VaryByHeader,
		}
		settings.SetDefaults()
		trusted := s.Trusted
		if trusted == nil {
			trusted = []string{}
		}
		rl, err := app.NewRateLimiter(&settings, trusted)
		if err != nil {
			return nil, err
		}
		for _, kr := range keyRequests {
			var raw strings.Builder
			target := "/api/v4/users/login"
			if kr.Query != "" {
				target += "?" + kr.Query
			}
			raw.WriteString("POST " + target + " HTTP/1.1\r\nHost: example.com\r\n")
			for _, kv := range kr.Headers {
				raw.WriteString(kv[0] + ": " + kv[1] + "\r\n")
			}
			raw.WriteString("\r\n")
			r, err := http.ReadRequest(bufio.NewReader(strings.NewReader(raw.String())))
			if err != nil {
				return nil, fmt.Errorf("%s: %w", kr.Name, err)
			}
			r.RemoteAddr = kr.RemoteAddr
			headers := kr.Headers
			if headers == nil {
				headers = [][2]string{}
			}
			out = append(out, map[string]any{
				"settings": map[string]any{
					"vary_by_user":        s.VaryByUser,
					"vary_by_remote_addr": s.VaryByRemoteAddr,
					"vary_by_header":      s.VaryByHeader,
					"trusted":             trusted,
				},
				"name":        kr.Name,
				"headers":     headers,
				"target":      target,
				"remote_addr": kr.RemoteAddr,
				"want":        rl.GenerateKey(r),
			})
		}
	}
	return out, nil
}

func writeRateLimitBehaviourFixture(outDir string) error {
	gcra := make([]map[string]any, 0, len(gcraCorpus))
	for _, c := range gcraCorpus {
		rows, err := runGCRACase(c)
		if err != nil {
			return fmt.Errorf("gcra %s: %w", c.Name, err)
		}
		gcra = append(gcra, map[string]any{
			"name":       c.Name,
			"per_sec":    c.PerSec,
			"max_burst":  c.MaxBurst,
			"store_size": c.StoreSize,
			"steps":      rows,
		})
	}
	writer := make([]map[string]any, 0, len(writerCorpus))
	for _, c := range writerCorpus {
		rows, err := runWriterCase(c)
		if err != nil {
			return fmt.Errorf("writer %s: %w", c.Name, err)
		}
		writer = append(writer, map[string]any{
			"name":      c.Name,
			"per_sec":   c.PerSec,
			"max_burst": c.MaxBurst,
			"responses": rows,
		})
	}
	keys, err := runKeyCorpus()
	if err != nil {
		return err
	}
	return writeJSONFixture(outDir, "behaviour_ratelimit.json", map[string]any{
		"gcra":         gcra,
		"writer":       writer,
		"generate_key": keys,
	})
}
