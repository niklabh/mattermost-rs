package main

// Behavioural oracle for the three session props `DoLogin` derives from the request's
// `User-Agent` (app/login.go:186-193), written to fixtures/behaviour_user_agent.json.
//
// # Why this needs an oracle at all
//
// `platform`, `os` and `browser` are on the wire: they are stored in `Sessions.Props` and echoed
// verbatim by `GET /users/{user_id}/sessions`, which this server already answers. They are
// produced by four small mapping functions in `channels/app/user_agent.go` layered over
// `github.com/avct/uasurfer`, a ~600-line pile of substring heuristics. Nothing about it is
// guessable, and a port written by reading it would be confidently wrong on the cases that
// matter.
//
// # What is real here and what is transcribed
//
//   - **The parser is the real thing.** `uasurfer.Parse` is called directly, so `parsed` below is
//     Go's own answer for every corpus string. That is the part a reader cannot reproduce.
//   - **The four mapping functions are transcribed**, character for character from
//     `channels/app/user_agent.go`, because they are unexported and `reference/` is read-only.
//     They are short, entirely table-driven, and the transcription is visible beside the Go it
//     came from. A bug shared between this copy and the Rust port would hide here, which is why
//     the Rust side also has an end-to-end check against the live Go server recorded in the
//     session report rather than only this fixture.
//
// Derived from the AGPL half of the tree, so this feeds `mm-app`'s tests, not `mm-model`'s — the
// rule behaviour_password.go states ([D-031]).

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/avct/uasurfer"
)

// userAgentCorpus is chosen for the decisions a reader gets wrong, not for coverage of the
// browser market:
//
//   - The empty string and a `curl/` UA are the two the parity suite itself sends, and they are
//     *different*: the empty string takes `parse`'s first case and never calls `evalOS`, while
//     `curl/8.5.0` falls all the way through `evalOS`'s default arm. Both end at
//     platform "Unknown", os "" — so only the browser version tells them apart, and it does not.
//   - The Mattermost desktop, mobile and mmctl agents, which MM's own mapping short-circuits on
//     *before* consulting uasurfer's browser name — but whose platform and os still come from it.
//   - `Mattermost Mobile/` with no recognisable OS, which is the one case where MM's fallbacks in
//     `getPlatformName`/`getOSName` actually fire.
//   - Edge, which uasurfer reports as `BrowserIE`, and which MM then renames to "Edge" only when
//     the major version exceeds 11 — so the *version* decides the *name*.
//   - Windows NT 6.1/6.3/10.0, because MM maps NT versions to marketing names by hand and the
//     table has gaps (NT 6.4, NT 11) that fall through to a bare "Windows".
//   - Safari on iOS, where the browser version is copied from the *OS* version and bumped by one
//     below major 4.
//   - A UA with no parenthesised platform group at all, and one where ')' precedes '(' — both
//     take `evalOS`'s index fix-up rather than its normal path.
//   - A non-ASCII UA, which takes `normalise`'s `strings.ToLower` fallback instead of the
//     byte-wise lowering.
var userAgentCorpus = []string{
	"",
	"curl/8.5.0",
	"Go-http-client/1.1",
	"mmctl/10.5.0",
	"Mattermost/5.10.0 Chrome/126.0.6478.127 Electron/31.2.0 Safari/537.36",
	"Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Mattermost/5.10.0 Chrome/126.0.6478.127 Electron/31.2.0 Safari/537.36",
	"Mattermost Mobile/2.19.0",
	"Mattermost Mobile/2.19.0 (iPhone; iOS 17.5.1)",
	"Mattermost Mobile/2.19.0 (Linux; Android 14)",
	"Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36",
	"Mozilla/5.0 (Windows NT 6.1; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/49.0.2623.112 Safari/537.36",
	"Mozilla/5.0 (Windows NT 6.3; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/70.0.3538.77 Safari/537.36",
	"Mozilla/5.0 (Windows NT 6.4; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/70.0.3538.77 Safari/537.36",
	"Mozilla/5.0 (Windows NT 6.0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/70.0.3538.77 Safari/537.36",
	"Mozilla/5.0 (Windows NT 6.2) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/70.0.3538.77 Safari/537.36",
	"Mozilla/5.0 (Windows NT 5.1) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/49.0.2623.112 Safari/537.36",
	"Mozilla/5.0 (Windows NT 5.0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/49.0.2623.112 Safari/537.36",
	"Mozilla/5.0 (Windows NT 5.2) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/49.0.2623.112 Safari/537.36",
	"Mozilla/5.0 (Windows NT 11.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36",
	"Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36 Edg/127.0.2651.86",
	"Mozilla/5.0 (Windows NT 10.0; WOW64; Trident/7.0; rv:11.0) like Gecko",
	"Mozilla/5.0 (compatible; MSIE 9.0; Windows NT 6.1; Trident/5.0)",
	"Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15",
	"Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36",
	"Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:128.0) Gecko/20100101 Firefox/128.0",
	"Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36",
	"Mozilla/5.0 (X11; Ubuntu; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
	"Mozilla/5.0 (X11; CrOS x86_64 14541.0.0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36",
	"Mozilla/5.0 (iPhone; CPU iPhone OS 17_5_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1",
	"Mozilla/5.0 (iPhone; CPU iPhone OS 3_1_3 like Mac OS X) AppleWebKit/528.18 (KHTML, like Gecko) Version/4.0 Mobile/7E18 Safari/528.16",
	"Mozilla/5.0 (iPad; CPU OS 17_5_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1",
	"Mozilla/5.0 (iPod touch; CPU iPhone OS 12_5_7 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/12.1 Mobile/15E148 Safari/604.1",
	"Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/127.0.6533.107 Mobile/15E148 Safari/604.1",
	"Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Mobile Safari/537.36",
	"Mozilla/5.0 (Linux; Android 4.4.2; SM-G900F Build/KOT49H) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/33.0.0.0 Mobile Safari/537.36",
	"Mozilla/5.0 (Linux; U; Android 4.0.3; en-us; KFTT Build/IML74K) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Safari/537.36",
	"Mozilla/5.0 (Android 14; Mobile; rv:128.0) Gecko/128.0 Firefox/128.0",
	"Mozilla/5.0 (Windows Phone 10.0; Android 6.0.1; Microsoft; Lumia 950) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/52.0.2743.116 Mobile Safari/537.36 Edge/15.15063",
	"Mozilla/5.0 (BB10; Touch) AppleWebKit/537.10+ (KHTML, like Gecko) Version/10.0.9.2372 Mobile Safari/537.10+",
	"Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 OPR/112.0.0.0",
	"Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
	"Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36",
	"Chrome/127.0.0.0 (Windows NT 10.0)",
	")Windows NT 10.0(",
	"Mozilla/5.0 (Wíndows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36",
	"Franz/5.9.2 Chrome/91.0.4472.164 Electron/13.6.9 Safari/537.36",
	"Mattermost Mobile/2.19.0 extra fields here",
	"Mattermost/",
	"Mattermost Mobile/",
	// `Version.parse` torture, reached through `evalMacintosh`s "os x " marker so that the
	// oracle answers for the digit walk itself rather than for a whole-agent guess.
	"Mozilla/5.0 (Macintosh; Intel Mac OS X 007)",
	"Mozilla/5.0 (Macintosh; Intel Mac OS X 01)",
	"Mozilla/5.0 (Macintosh; Intel Mac OS X 0)",
	"Mozilla/5.0 (Macintosh; Intel Mac OS X 10)",
	"Mozilla/5.0 (Macintosh; Intel Mac OS X 1.2.3.4)",
	"Mozilla/5.0 (Macintosh; Intel Mac OS X x)",
	// Safari with **no** `version/` token, which is the only way to reach `evalBrowserVersion`s
	// Safari arm — it copies the *OS* version and adds one below major 4. Every other Safari
	// string in this corpus carries `version/` and returns before that arm.
	"Mozilla/5.0 (iPhone; CPU iPhone OS 3_1_3 like Mac OS X) AppleWebKit/528.18 (KHTML, like Gecko) Mobile/7E18 Safari/528.16",
	"Mozilla/5.0 (iPhone; CPU iPhone OS 17_5_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148 Safari/604.1",
	// NT 10.**1**: the Windows name table matches `major == 10` and ignores the minor, so this
	// is "Windows 10" too. Nothing else in the corpus separates that arm from a `(10, 0)` one.
	"Mozilla/5.0 (Windows NT 10.1; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36",
	// `evalOS`s index fix-up, with an answer that actually turns on it. `)Windows NT 10.0(`
	// above takes the same branch and reaches the same place either way, because the default
	// chain re-reads the *whole* string; this one is decided by `agentPlatform` alone —
	// `strings.Contains(agentPlatform, "windows phone ")` — so dropping the fix-up moves it from
	// Windows Phone to a bare Windows. Without this row the fix-up is unfalsifiable.
	")x(Windows Phone 8.0; Trident/5.0",
}

// userAgentCase is one corpus string with everything both layers produce for it.
type userAgentCase struct {
	UserAgent string `json:"user_agent"`

	// The raw uasurfer answer — the part this file does not transcribe.
	Platform       string `json:"platform_enum"`
	OSName         string `json:"os_name_enum"`
	OSVersion      [3]int `json:"os_version"`
	BrowserName    string `json:"browser_name_enum"`
	BrowserVersion [3]int `json:"browser_version"`

	// What `DoLogin` actually writes into `Sessions.Props`.
	PropPlatform string `json:"prop_platform"`
	PropOS       string `json:"prop_os"`
	PropBrowser  string `json:"prop_browser"`

	// `utils.IsMobileRequest` (channels/utils/utils.go:232), which decides whether the session
	// takes the mobile length. Not a uasurfer call at all — a five-keyword substring scan on the
	// *raw* string — and it disagrees with the parsed platform often enough to be worth pinning.
	IsMobileRequest bool `json:"is_mobile_request"`
}

// ---------------------------------------------------------------------------------------------
// Transcribed from channels/app/user_agent.go. Keep character for character.
// ---------------------------------------------------------------------------------------------

const maxUserAgentVersionLength = 128

var platformNames = map[uasurfer.Platform]string{
	uasurfer.PlatformUnknown:      "Unknown",
	uasurfer.PlatformWindows:      "Windows",
	uasurfer.PlatformMac:          "Macintosh",
	uasurfer.PlatformLinux:        "Linux",
	uasurfer.PlatformiPad:         "iPad",
	uasurfer.PlatformiPhone:       "iPhone",
	uasurfer.PlatformiPod:         "iPod",
	uasurfer.PlatformBlackberry:   "BlackBerry",
	uasurfer.PlatformWindowsPhone: "Windows Phone",
}

func getPlatformName(ua *uasurfer.UserAgent, userAgentString string) string {
	platform := ua.OS.Platform

	if platform == uasurfer.PlatformUnknown && strings.Contains(userAgentString, "Mattermost Mobile/") {
		if strings.Contains(userAgentString, "iPhone") {
			platform = uasurfer.PlatformiPhone
		} else if strings.Contains(userAgentString, "iPad") {
			platform = uasurfer.PlatformiPad
		} else {
			platform = uasurfer.PlatformLinux
		}
	}

	name, ok := platformNames[platform]
	if !ok {
		return platformNames[uasurfer.PlatformUnknown]
	}
	return name
}

var osNames = map[uasurfer.OSName]string{
	uasurfer.OSUnknown:      "",
	uasurfer.OSWindowsPhone: "Windows Phone",
	uasurfer.OSWindows:      "Windows",
	uasurfer.OSMacOSX:       "Mac OS",
	uasurfer.OSiOS:          "iOS",
	uasurfer.OSAndroid:      "Android",
	uasurfer.OSBlackberry:   "BlackBerry",
	uasurfer.OSChromeOS:     "Chrome OS",
	uasurfer.OSKindle:       "Kindle",
	uasurfer.OSWebOS:        "webOS",
	uasurfer.OSLinux:        "Linux",
}

func getOSName(ua *uasurfer.UserAgent, userAgentString string) string {
	os := ua.OS

	if os.Name == uasurfer.OSWindows {
		major := os.Version.Major
		minor := os.Version.Minor

		switch {
		case major == 5 && minor == 0:
			return "Windows 2000"
		case major == 5 && minor == 1:
			return "Windows XP"
		case major == 5 && minor == 2:
			return "Windows XP x64 Edition"
		case major == 6 && minor == 0:
			return "Windows Vista"
		case major == 6 && minor == 1:
			return "Windows 7"
		case major == 6 && minor == 2:
			return "Windows 8"
		case major == 6 && minor == 3:
			return "Windows 8.1"
		case major == 10:
			return "Windows 10"
		default:
			return "Windows"
		}
	}

	osName := os.Name

	if osName == uasurfer.OSUnknown && strings.Contains(userAgentString, "Mattermost Mobile/") {
		if strings.Contains(userAgentString, "iPhone") {
			osName = uasurfer.OSiOS
		} else if strings.Contains(userAgentString, "iPad") {
			osName = uasurfer.OSiOS
		} else {
			osName = uasurfer.OSAndroid
		}
	}

	name, ok := osNames[osName]
	if ok {
		return name
	}

	return osNames[uasurfer.OSUnknown]
}

const desktopAppVersionPrefix = "Mattermost/"

var versionPrefixes = []string{
	"Mattermost Mobile/",
	desktopAppVersionPrefix,
	"mmctl/",
	"Franz/",
}

func getBrowserVersion(ua *uasurfer.UserAgent, userAgentString string) string {
	for _, prefix := range versionPrefixes {
		if _, after, ok := strings.Cut(userAgentString, prefix); ok {
			if fields := strings.Fields(after); len(fields) > 0 {
				return limitStringLength(fields[0], maxUserAgentVersionLength)
			}
		}
	}
	return getUAVersion(ua.Browser.Version)
}

func limitStringLength(field string, limit int) string {
	endPos := min(len(field), limit)
	return field[:endPos]
}

func getUAVersion(version uasurfer.Version) string {
	if version.Patch == 0 {
		return fmt.Sprintf("%v.%v", version.Major, version.Minor)
	}
	return fmt.Sprintf("%v.%v.%v", version.Major, version.Minor, version.Patch)
}

var browserNames = map[uasurfer.BrowserName]string{
	uasurfer.BrowserUnknown:    "Unknown",
	uasurfer.BrowserChrome:     "Chrome",
	uasurfer.BrowserIE:         "Internet Explorer",
	uasurfer.BrowserSafari:     "Safari",
	uasurfer.BrowserFirefox:    "Firefox",
	uasurfer.BrowserAndroid:    "Android",
	uasurfer.BrowserOpera:      "Opera",
	uasurfer.BrowserBlackberry: "BlackBerry",
}

func getBrowserName(ua *uasurfer.UserAgent, userAgentString string) string {
	browser := ua.Browser.Name

	if strings.Contains(userAgentString, "Electron") ||
		(strings.Contains(userAgentString, "Mattermost") && !strings.Contains(userAgentString, "Mattermost Mobile")) {
		return "Desktop App"
	}

	if strings.Contains(userAgentString, "Mattermost Mobile") {
		return "Mobile App"
	}

	if strings.Contains(userAgentString, "mmctl") {
		return "mmctl"
	}

	if browser == uasurfer.BrowserIE && ua.Browser.Version.Major > 11 {
		return "Edge"
	}

	if name, ok := browserNames[browser]; ok {
		return name
	}

	return browserNames[uasurfer.BrowserUnknown]
}

// isMobileRequest is channels/utils/utils.go:232, transcribed.
func isMobileRequest(userAgent string) bool {
	if userAgent == "" {
		return false
	}

	mobileKeywords := []string{"Mobile", "Android", "iOS", "iPhone", "iPad"}
	for _, keyword := range mobileKeywords {
		if strings.Contains(userAgent, keyword) {
			return true
		}
	}

	return false
}

// ---------------------------------------------------------------------------------------------

func writeUserAgentBehaviourFixture(outDir string) error {
	cases := make([]userAgentCase, 0, len(userAgentCorpus))
	for _, raw := range userAgentCorpus {
		ua := uasurfer.Parse(raw)
		cases = append(cases, userAgentCase{
			UserAgent:       raw,
			Platform:        ua.OS.Platform.StringTrimPrefix(),
			OSName:          ua.OS.Name.StringTrimPrefix(),
			OSVersion:       [3]int{ua.OS.Version.Major, ua.OS.Version.Minor, ua.OS.Version.Patch},
			BrowserName:     ua.Browser.Name.StringTrimPrefix(),
			BrowserVersion:  [3]int{ua.Browser.Version.Major, ua.Browser.Version.Minor, ua.Browser.Version.Patch},
			PropPlatform:    getPlatformName(ua, raw),
			PropOS:          getOSName(ua, raw),
			PropBrowser:     fmt.Sprintf("%v/%v", getBrowserName(ua, raw), getBrowserVersion(ua, raw)),
			IsMobileRequest: isMobileRequest(raw),
		})
	}

	blob, err := json.MarshalIndent(map[string]any{"user_agent": cases}, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(
		filepath.Join(outDir, "behaviour_user_agent.json"),
		append(blob, '\n'),
		0o644,
	)
}
