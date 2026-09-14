package main

// Behavioural oracle for shared/httpservice/client.go's `IsReservedIP`, written to
// fixtures/behaviour_httpservice.json.
//
// The reserved-range table is thirty CIDRs, sixteen IPv4 and fourteen IPv6, and a port that
// transcribes it by hand can drop one, widen one, or apply the IPv4 ranges to an IPv4-mapped
// IPv6 address the wrong way round — `IsReservedIP` first does `ip.To4()`, so `::ffff:10.0.0.1`
// is the same answer as `10.0.0.1`. The corpus walks every range at its first address, its last,
// and one past its end, plus the mapped forms and a handful of ordinary public addresses.
//
// `IsOwnIP` and the unexported `checkInternalIP` are not here: the first depends on the machine's
// interfaces and the second is not exported. Determinism: fixed values only — see [D-032].

import (
	"encoding/json"
	"net"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/shared/httpservice"
)

type reservedIPCase struct {
	IP       string `json:"ip"`
	Reserved bool   `json:"reserved"`
}

func writeHTTPServiceBehaviourFixture(outDir string) error {
	ips := []string{
		// IPv4 ranges: first, last, one past.
		"10.0.0.0", "10.255.255.255", "11.0.0.0",
		"172.16.0.0", "172.31.255.255", "172.32.0.0", "172.15.255.255",
		"192.168.0.0", "192.168.255.255", "192.169.0.0",
		"127.0.0.0", "127.255.255.255", "128.0.0.0", "127.0.0.1",
		"0.0.0.0", "0.255.255.255", "1.0.0.0",
		"169.254.0.0", "169.254.255.255", "169.255.0.0",
		"192.0.0.0", "192.0.0.255", "192.0.1.0",
		"192.0.2.0", "192.0.2.255", "192.0.3.0",
		"198.51.100.0", "198.51.100.255", "198.51.101.0",
		"203.0.113.0", "203.0.113.255", "203.0.114.0",
		"192.88.99.0", "192.88.99.255", "192.88.100.0",
		"198.18.0.0", "198.19.255.255", "198.20.0.0", "198.17.255.255",
		"224.0.0.0", "239.255.255.255", "240.0.0.0", "255.255.255.254", "255.255.255.255",
		"100.64.0.0", "100.127.255.255", "100.128.0.0", "100.63.255.255",
		// Ordinary public addresses.
		"8.8.8.8", "1.1.1.1", "93.184.216.34", "151.101.1.69",
		// IPv6 ranges: first, last, one past.
		"::", "::1", "::2",
		"100::", "100::ffff:ffff:ffff:ffff", "100:0:0:1::",
		"2001::", "2001:1ff:ffff:ffff:ffff:ffff:ffff:ffff", "2001:200::",
		"2001:2::", "2001:2:0:ffff:ffff:ffff:ffff:ffff", "2001:2:1::",
		"2001:db8::", "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff", "2001:db9::",
		"2001:10::", "2001:1f:ffff:ffff:ffff:ffff:ffff:ffff", "2001:20::", "2001:2f:ffff:ffff:ffff:ffff:ffff:ffff", "2001:30::",
		"fc00::", "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", "fe00::",
		"fe80::", "febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff", "fec0::",
		"ff00::", "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
		"2002::", "2002:ffff:ffff:ffff:ffff:ffff:ffff:ffff", "2003::",
		"64:ff9b::", "64:ff9b::ffff:ffff", "64:ff9b::1:0:0",
		// Public IPv6.
		"2606:4700:4700::1111", "2a00:1450:4001:80e::200e",
		// IPv4-mapped IPv6: `To4()` succeeds, so these answer as their IPv4 halves.
		"::ffff:10.0.0.1", "::ffff:8.8.8.8", "::ffff:127.0.0.1", "::ffff:192.168.1.1",
	}
	cases := make([]reservedIPCase, 0, len(ips))
	for _, raw := range ips {
		ip := net.ParseIP(raw)
		if ip == nil {
			panic("behaviour_httpservice: unparseable IP " + raw)
		}
		cases = append(cases, reservedIPCase{IP: raw, Reserved: httpservice.IsReservedIP(ip)})
	}
	out := map[string]any{"is_reserved_ip": cases}
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_httpservice.json"), append(data, '\n'), 0o644)
}
