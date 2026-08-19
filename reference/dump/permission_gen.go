package main

// Generator for crates/mm-model/src/permission_generated.rs, and the behavioural oracle for
// model/permission.go, written to fixtures/behaviour_permission.json.
//
// permission.go is on MIGRATION.md's out-of-scope list with the reason "generate from Go, do not
// hand-translate": 310 permissions × four string fields is 1,240 opportunities to mistype a
// literal that nothing downstream would notice, because a wrong `Id` fails open — the permission
// simply never matches, and the route silently denies (or grants) forever.
//
// # Why this generator reads the AST as well as the runtime values
//
// The tables (`AllPermissions`, `DeprecatedPermissions`, …) are ordinary package-level slices, so
// their *values* come from the linked package at runtime and need no parsing. What runtime access
// cannot give is the **identifier** each permission was declared under — Go has no reflection over
// package-level vars — and the identifier is what every one of the 674 `SessionHasPermission*`
// call sites names. Worse, a permission that is declared but appears in no table is invisible from
// the runtime side entirely, which is exactly the shape [D-120] found in job.go (42 declared job
// types, 24 reachable).
//
// So: the literals are read from `initializePermissions` with go/parser, the tables are read from
// the linked package, and the two are cross-checked against each other. Every runtime permission
// must match an AST literal field-for-field, or the generator fails rather than emitting a table
// that agrees with neither.
//
// # Naming
//
// The Rust statics are named from the **id**, not from the Go identifier: `PERMISSION_` +
// uppercased id. The id is the value that crosses the wire and sits in the Roles.Permissions
// column, so deriving the name from it means a name can never disagree with the thing it names.
// Where the Go identifier's own snake-case would differ, the divergence is recorded in the
// fixture under ident_id_mismatches rather than silently resolved, and every static carries a doc
// comment naming its Go identifier so a port of a `SessionHasPermissionTo(model.PermissionX)`
// call site can be found by grep in either direction.
//
// Determinism: no clock, no randomness, no host lookup. The literals are compile-time constants
// and the tables are built by a single `init()`.

import (
	"encoding/json"
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"unicode"

	"github.com/mattermost/mattermost/server/public/model"
)

// permissionGoPath is relative to reference/dump, which is where the generator is run from.
const permissionGoPath = "../mattermost/server/public/model/permission.go"

// permissionIDPattern is what the id half of the Rust name is derived from. Anything outside it
// would not survive uppercasing into a Rust identifier, so a violation fails the run.
var permissionIDPattern = regexp.MustCompile(`^[a-z][a-z0-9_]*$`)

type permissionLiteral struct {
	Ident       string `json:"ident"`
	Line        int    `json:"line"`
	RustName    string `json:"rust_name"`
	ID          string `json:"id"`
	Name        string `json:"name"`
	Description string `json:"description"`
	Scope       string `json:"scope"`
	ScopeIdent  string `json:"scope_ident"`
}

type permissionErrorCase struct {
	Name        string   `json:"name"`
	UserID      string   `json:"user_id"`
	Permissions []string `json:"permissions"`
	Where       string   `json:"where"`
	ID          string   `json:"id"`
	Message     string   `json:"message"`
	Detailed    string   `json:"detailed_error"`
	StatusCode  int      `json:"status_code"`
	ToJSON      string   `json:"to_json"`
}

func writePermissionGenerated(outDir, rustOutDir string) error {
	literals, scopes, declaredIdents, err := parsePermissionSource(permissionGoPath)
	if err != nil {
		return err
	}
	if len(literals) == 0 {
		return fmt.Errorf("parsed no permission literals from %s", permissionGoPath)
	}
	if len(literals) != len(declaredIdents) {
		// A declared-but-never-assigned var would be a nil *Permission that panics on first use;
		// an assigned-but-undeclared one cannot compile. Either way the two counts must agree.
		return fmt.Errorf("%d `var Permission* *Permission` declarations but %d assignments in initializePermissions",
			len(declaredIdents), len(literals))
	}

	byID := make(map[string]permissionLiteral, len(literals))
	byRustName := make(map[string]string, len(literals))
	for _, p := range literals {
		if prev, dup := byID[p.ID]; dup {
			return fmt.Errorf("permission id %q declared twice: %s and %s", p.ID, prev.Ident, p.Ident)
		}
		byID[p.ID] = p
		if prev, dup := byRustName[p.RustName]; dup {
			return fmt.Errorf("rust name %s collides: %s and %s", p.RustName, prev, p.Ident)
		}
		byRustName[p.RustName] = p.Ident
	}

	// The cross-check that makes the AST read trustworthy: every permission the linked package
	// actually built must match a parsed literal in all four fields.
	runtimeTables := map[string][]*model.Permission{
		"AllPermissions":               model.AllPermissions,
		"DeprecatedPermissions":        model.DeprecatedPermissions,
		"SysconsoleReadPermissions":    model.SysconsoleReadPermissions,
		"SysconsoleWritePermissions":   model.SysconsoleWritePermissions,
		"ModeratedBookmarkPermissions": model.ModeratedBookmarkPermissions,
	}
	seenAtRuntime := map[string]bool{}
	for table, perms := range runtimeTables {
		for i, p := range perms {
			if p == nil {
				return fmt.Errorf("%s[%d] is nil", table, i)
			}
			lit, ok := byID[p.Id]
			if !ok {
				return fmt.Errorf("%s[%d] has id %q, which no literal in initializePermissions declares", table, i, p.Id)
			}
			if lit.Name != p.Name || lit.Description != p.Description || lit.Scope != p.Scope {
				return fmt.Errorf("%s[%d] (%s) disagrees with the parsed literal:\n  runtime %q %q %q\n  parsed  %q %q %q",
					table, i, p.Id, p.Name, p.Description, p.Scope, lit.Name, lit.Description, lit.Scope)
			}
			seenAtRuntime[p.Id] = true
		}
	}

	rust, err := renderPermissionsRust(literals, scopes)
	if err != nil {
		return err
	}
	rustPath := filepath.Join(rustOutDir, "permission_generated.rs")
	if err := os.WriteFile(rustPath, []byte(rust), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s (%d permissions)\n", rustPath, len(literals))

	return writePermissionBehaviourFixture(outDir, literals, scopes, seenAtRuntime)
}

// --- parsing ------------------------------------------------------------------

func parsePermissionSource(path string) (literals []permissionLiteral, scopes map[string]string, declaredIdents []string, err error) {
	fset := token.NewFileSet()
	file, err := parser.ParseFile(fset, path, nil, 0)
	if err != nil {
		return nil, nil, nil, fmt.Errorf("parsing %s: %w", path, err)
	}

	scopes = map[string]string{}
	for _, decl := range file.Decls {
		gen, ok := decl.(*ast.GenDecl)
		if !ok {
			continue
		}
		for _, spec := range gen.Specs {
			value, ok := spec.(*ast.ValueSpec)
			if !ok {
				continue
			}
			switch gen.Tok {
			case token.CONST:
				for i, name := range value.Names {
					if !strings.HasPrefix(name.Name, "PermissionScope") || i >= len(value.Values) {
						continue
					}
					lit, ok := value.Values[i].(*ast.BasicLit)
					if !ok || lit.Kind != token.STRING {
						continue
					}
					unquoted, uerr := strconv.Unquote(lit.Value)
					if uerr != nil {
						return nil, nil, nil, fmt.Errorf("unquoting %s: %w", name.Name, uerr)
					}
					scopes[name.Name] = unquoted
				}
			case token.VAR:
				// `var PermissionInviteUser *Permission` — type present, no value.
				star, ok := value.Type.(*ast.StarExpr)
				if !ok {
					continue
				}
				ident, ok := star.X.(*ast.Ident)
				if !ok || ident.Name != "Permission" {
					continue
				}
				for _, name := range value.Names {
					declaredIdents = append(declaredIdents, name.Name)
				}
			}
		}
	}
	if len(scopes) == 0 {
		return nil, nil, nil, fmt.Errorf("no PermissionScope* constants found in %s", path)
	}

	var body *ast.BlockStmt
	for _, decl := range file.Decls {
		fn, ok := decl.(*ast.FuncDecl)
		if ok && fn.Recv == nil && fn.Name.Name == "initializePermissions" {
			body = fn.Body
			break
		}
	}
	if body == nil {
		return nil, nil, nil, fmt.Errorf("no initializePermissions() in %s", path)
	}

	for _, stmt := range body.List {
		assign, ok := stmt.(*ast.AssignStmt)
		if !ok || len(assign.Lhs) != 1 || len(assign.Rhs) != 1 {
			continue
		}
		target, ok := assign.Lhs[0].(*ast.Ident)
		if !ok || !strings.HasPrefix(target.Name, "Permission") {
			continue
		}
		unary, ok := assign.Rhs[0].(*ast.UnaryExpr)
		if !ok || unary.Op != token.AND {
			continue
		}
		composite, ok := unary.X.(*ast.CompositeLit)
		if !ok {
			continue
		}
		typeIdent, ok := composite.Type.(*ast.Ident)
		if !ok || typeIdent.Name != "Permission" {
			continue
		}
		// Every literal in the file is positional and four-wide. A keyed one would silently
		// misplace fields here, so reject it rather than guess.
		if len(composite.Elts) != 4 {
			return nil, nil, nil, fmt.Errorf("%s: expected 4 positional fields, got %d", target.Name, len(composite.Elts))
		}
		strs := make([]string, 3)
		for i := 0; i < 3; i++ {
			lit, ok := composite.Elts[i].(*ast.BasicLit)
			if !ok || lit.Kind != token.STRING {
				return nil, nil, nil, fmt.Errorf("%s: field %d is not a string literal", target.Name, i)
			}
			unquoted, uerr := strconv.Unquote(lit.Value)
			if uerr != nil {
				return nil, nil, nil, fmt.Errorf("%s: unquoting field %d: %w", target.Name, i, uerr)
			}
			strs[i] = unquoted
		}
		scopeIdent, ok := composite.Elts[3].(*ast.Ident)
		if !ok {
			return nil, nil, nil, fmt.Errorf("%s: scope is not an identifier", target.Name)
		}
		scope, ok := scopes[scopeIdent.Name]
		if !ok {
			return nil, nil, nil, fmt.Errorf("%s: unknown scope constant %s", target.Name, scopeIdent.Name)
		}
		if !permissionIDPattern.MatchString(strs[0]) {
			return nil, nil, nil, fmt.Errorf("%s: id %q is not a lowercase identifier", target.Name, strs[0])
		}
		literals = append(literals, permissionLiteral{
			Ident:       target.Name,
			Line:        fset.Position(target.Pos()).Line,
			RustName:    "PERMISSION_" + strings.ToUpper(strs[0]),
			ID:          strs[0],
			Name:        strs[1],
			Description: strs[2],
			Scope:       scope,
			ScopeIdent:  scopeIdent.Name,
		})
	}

	return literals, scopes, declaredIdents, nil
}

// --- Rust emission ------------------------------------------------------------

func renderPermissionsRust(literals []permissionLiteral, scopes map[string]string) (string, error) {
	var b strings.Builder
	fmt.Fprintf(&b, `//! @generated by `+"`reference/dump/permission_gen.go`"+` from `+"`model/permission.go`"+`.
//! DO NOT EDIT — re-run `+"`cd reference/dump && go run .`"+` instead.
//!
//! The %d permissions Mattermost's authorization system is built out of, and the seven tables
//! that group them. `+"`MIGRATION.md`"+` lists permission.go as generate-only for the reason every
//! table here makes concrete: a mistyped id fails **open** — it matches no role, so the check it
//! guards answers the same way forever and nothing reports an error.
//!
//! Names are derived from the `+"`Id`"+`, which is the value stored in `+"`Roles.Permissions`"+` and sent
//! over the wire, so a name cannot disagree with what it names. Each static's doc comment carries
//! the Go identifier the 674 `+"`SessionHasPermission*`"+` call sites use.
//!
//! Every table carries `+"`#[rustfmt::skip]`"+` so `+"`cargo fmt`"+` and the generator stay idempotent
//! against each other, the same as `+"`emoji_generated.rs`"+`.

use std::borrow::Cow;

#[rustfmt::skip]
use crate::permission::{
    Permission,
`, len(literals))
	scopeIdents := make([]string, 0, len(scopes))
	for ident := range scopes {
		scopeIdents = append(scopeIdents, ident)
	}
	sort.Strings(scopeIdents)
	for _, ident := range scopeIdents {
		fmt.Fprintf(&b, "    %s,\n", screamingSnake(ident))
	}
	b.WriteString("};\n\n")

	for _, p := range literals {
		fmt.Fprintf(&b, "/// `model.%s` (permission.go:%d).\n", p.Ident, p.Line)
		// Skipped for the same reason the tables are: several i18n keys push the line past 100
		// columns, and rustfmt would wrap them into a shape the generator then rewrites.
		fmt.Fprintf(&b, "#[rustfmt::skip]\npub static %s: Permission = Permission {\n", p.RustName)
		fmt.Fprintf(&b, "    id: Cow::Borrowed(%s),\n", strconv.Quote(p.ID))
		fmt.Fprintf(&b, "    name: Cow::Borrowed(%s),\n", strconv.Quote(p.Name))
		fmt.Fprintf(&b, "    description: Cow::Borrowed(%s),\n", strconv.Quote(p.Description))
		fmt.Fprintf(&b, "    scope: Cow::Borrowed(%s),\n", screamingSnake(p.ScopeIdent))
		b.WriteString("};\n\n")
	}

	byID := make(map[string]permissionLiteral, len(literals))
	for _, p := range literals {
		byID[p.ID] = p
	}
	emitTable := func(rustName, goName, doc string, perms []*model.Permission) error {
		fmt.Fprintf(&b, "/// `model.%s` — %s\n", goName, doc)
		fmt.Fprintf(&b, "#[rustfmt::skip]\npub static %s: &[&Permission] = &[\n", rustName)
		for _, p := range perms {
			lit, ok := byID[p.Id]
			if !ok {
				return fmt.Errorf("%s contains unknown id %q", goName, p.Id)
			}
			fmt.Fprintf(&b, "    &%s,\n", lit.RustName)
		}
		b.WriteString("];\n\n")
		return nil
	}

	if err := emitTable("ALL_PERMISSIONS", "AllPermissions",
		"every permission any role may hold, in Go's order: the system scope minus sysconsole,\n/// then team, channel, sysconsole read, sysconsole write, group, playbook and run.",
		model.AllPermissions); err != nil {
		return "", err
	}
	if err := emitTable("DEPRECATED_PERMISSIONS", "DeprecatedPermissions",
		"kept so old rows in `Roles.Permissions` still parse. Disjoint from\n/// [`ALL_PERMISSIONS`]; see the behaviour fixture, which measures the overlap rather than\n/// asserting it.",
		model.DeprecatedPermissions); err != nil {
		return "", err
	}
	if err := emitTable("SYSCONSOLE_READ_PERMISSIONS", "SysconsoleReadPermissions",
		"the system-console read permissions, a subset of [`ALL_PERMISSIONS`].",
		model.SysconsoleReadPermissions); err != nil {
		return "", err
	}
	if err := emitTable("SYSCONSOLE_WRITE_PERMISSIONS", "SysconsoleWritePermissions",
		"the system-console write permissions, a subset of [`ALL_PERMISSIONS`].",
		model.SysconsoleWritePermissions); err != nil {
		return "", err
	}
	if err := emitTable("MODERATED_BOOKMARK_PERMISSIONS", "ModeratedBookmarkPermissions",
		"the eight channel-bookmark permissions that channel moderation collapses into the\n/// single `manage_bookmarks` control.",
		model.ModeratedBookmarkPermissions); err != nil {
		return "", err
	}

	fmt.Fprintf(&b, "/// `model.ChannelModeratedPermissions` — the five controls the channel-moderation UI\n"+
		"/// offers, in Go's order. These are **not** permission ids: `create_reactions`,\n"+
		"/// `manage_members` and `manage_bookmarks` name no permission and exist only here.\n"+
		"#[rustfmt::skip]\npub static CHANNEL_MODERATED_PERMISSIONS: &[&str] = &[\n")
	for _, s := range model.ChannelModeratedPermissions {
		fmt.Fprintf(&b, "    %s,\n", strconv.Quote(s))
	}
	b.WriteString("];\n\n")

	// Go's map has no order; emitting it sorted by key makes the table binary-searchable and the
	// generator idempotent. One caller does range it — role.go:713, inside
	// GetChannelModeratedPermissions — but only to find a key it already holds, and it writes into
	// a map, so no result depends on the iteration order. Checked at the pinned SHA rather than
	// assumed; a future caller that iterated for an ordered result would be reading an order Go
	// randomises per run, and sorting here would then be a divergence.
	keys := make([]string, 0, len(model.ChannelModeratedPermissionsMap))
	for k := range model.ChannelModeratedPermissionsMap {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	fmt.Fprintf(&b, "/// `model.ChannelModeratedPermissionsMap` — which moderation control a permission id\n"+
		"/// belongs to. Sorted by key so [`slice::binary_search_by_key`] applies. Go's map has no\n"+
		"/// order; its one ranging caller (`role.go:713`) uses the loop to find a key it already\n"+
		"/// holds and writes into a map, so no result depends on the order.\n"+
		"#[rustfmt::skip]\npub static CHANNEL_MODERATED_PERMISSIONS_MAP: &[(&str, &str)] = &[\n")
	for _, k := range keys {
		fmt.Fprintf(&b, "    (%s, %s),\n", strconv.Quote(k), strconv.Quote(model.ChannelModeratedPermissionsMap[k]))
	}
	b.WriteString("];\n")

	return b.String(), nil
}

// screamingSnake converts a Go identifier to the Rust constant name convention. It splits only on
// lower→upper transitions, deliberately NOT on the upper→upper→lower boundary the usual algorithm
// uses: that rule turns `PermissionScopeSystem` correctly but would render `OAuth` as `O_AUTH`.
// Only the six scope constants go through it — permission statics are named from their id.
func screamingSnake(ident string) string {
	var b strings.Builder
	runes := []rune(ident)
	for i, r := range runes {
		if i > 0 && unicode.IsUpper(r) && !unicode.IsUpper(runes[i-1]) {
			b.WriteByte('_')
		}
		b.WriteRune(unicode.ToUpper(r))
	}
	return b.String()
}

// --- behaviour fixture --------------------------------------------------------

func writePermissionBehaviourFixture(outDir string, literals []permissionLiteral, scopes map[string]string, seenAtRuntime map[string]bool) error {
	ids := func(perms []*model.Permission) []string {
		out := make([]string, 0, len(perms))
		for _, p := range perms {
			out = append(out, p.Id)
		}
		return out
	}

	inAll := map[string]bool{}
	var allDuplicates []string
	for _, p := range model.AllPermissions {
		if inAll[p.Id] {
			allDuplicates = append(allDuplicates, p.Id)
		}
		inAll[p.Id] = true
	}

	// Declared but in no table at all, and declared but not in AllPermissions. The first set is
	// unreachable from any role; the second is reachable only as a deprecated row.
	var declaredNotInAll, declaredInNoTable []string
	for _, p := range literals {
		if !inAll[p.ID] {
			declaredNotInAll = append(declaredNotInAll, p.ID)
		}
		if !seenAtRuntime[p.ID] {
			declaredInNoTable = append(declaredInNoTable, p.ID)
		}
	}

	var deprecatedInAll []string
	for _, p := range model.DeprecatedPermissions {
		if inAll[p.Id] {
			deprecatedInAll = append(deprecatedInAll, p.Id)
		}
	}

	scopeHistogram := map[string]int{}
	for _, p := range literals {
		scopeHistogram[p.Scope]++
	}

	// Where the Go identifier's own snake-case disagrees with the id the static is named from.
	var identIDMismatches []map[string]string
	for _, p := range literals {
		fromIdent := strings.ToLower(screamingSnake(strings.TrimPrefix(p.Ident, "Permission")))
		if fromIdent != p.ID {
			identIDMismatches = append(identIDMismatches, map[string]string{
				"ident": p.Ident, "id": p.ID, "ident_snake": fromIdent,
			})
		}
	}

	out := map[string]any{
		"permissions":                       literals,
		"scope_constants":                   scopes,
		"scope_histogram":                   scopeHistogram,
		"all_permissions":                   ids(model.AllPermissions),
		"deprecated_permissions":            ids(model.DeprecatedPermissions),
		"sysconsole_read_permissions":       ids(model.SysconsoleReadPermissions),
		"sysconsole_write_permissions":      ids(model.SysconsoleWritePermissions),
		"moderated_bookmark_permissions":    ids(model.ModeratedBookmarkPermissions),
		"channel_moderated_permissions":     model.ChannelModeratedPermissions,
		"channel_moderated_permissions_map": model.ChannelModeratedPermissionsMap,
		"declared_not_in_all":               orEmpty(declaredNotInAll),
		"declared_in_no_table":              orEmpty(declaredInNoTable),
		"deprecated_in_all":                 orEmpty(deprecatedInAll),
		"all_permissions_duplicates":        orEmpty(allDuplicates),
		"ident_id_mismatches":               identIDMismatches,
		"counts": map[string]int{
			"declared":            len(literals),
			"all_permissions":     len(model.AllPermissions),
			"deprecated":          len(model.DeprecatedPermissions),
			"sysconsole_read":     len(model.SysconsoleReadPermissions),
			"sysconsole_write":    len(model.SysconsoleWritePermissions),
			"moderated_bookmark":  len(model.ModeratedBookmarkPermissions),
			"channel_moderated":   len(model.ChannelModeratedPermissions),
			"channel_moderated_m": len(model.ChannelModeratedPermissionsMap),
		},
		"make_permission_error": makePermissionErrorAll(),
		"http_status_forbidden": http.StatusForbidden,
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, "behaviour_permission.json")
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}

func orEmpty(s []string) []string {
	if s == nil {
		return []string{}
	}
	return s
}

// makePermissionErrorAll probes MakePermissionErrorForUser, whose whole behaviour is one string
// join. The empty case is the interesting one: the detail ends in a bare "permission=" rather
// than being omitted, and a port that skips the prefix when there is nothing to join would be short by
// eleven bytes on a detail that ends up in server logs.
func makePermissionErrorAll() []permissionErrorCase {
	cases := []struct {
		name   string
		userID string
		perms  []*model.Permission
	}{
		{"none", "kb1qzjrbstrs3nmhmwq6mfrpbc", nil},
		{"empty_slice", "kb1qzjrbstrs3nmhmwq6mfrpbc", []*model.Permission{}},
		{"one", "kb1qzjrbstrs3nmhmwq6mfrpbc", []*model.Permission{model.PermissionManageTeam}},
		{"two", "kb1qzjrbstrs3nmhmwq6mfrpbc", []*model.Permission{model.PermissionCreatePost, model.PermissionEditPost}},
		{"three", "kb1qzjrbstrs3nmhmwq6mfrpbc", []*model.Permission{
			model.PermissionReadChannel, model.PermissionAddReaction, model.PermissionUploadFile,
		}},
		{"empty_user_id", "", []*model.Permission{model.PermissionManageSystem}},
		// The session overload is a one-line delegation, recorded so a port cannot get the two
		// out of step.
		{"via_session", "sessionuser1jbyqbtxbtqcgy3wa", []*model.Permission{model.PermissionJoinPublicTeams}},
	}

	out := make([]permissionErrorCase, 0, len(cases))
	for _, c := range cases {
		var appErr *model.AppError
		if c.name == "via_session" {
			appErr = model.MakePermissionError(&model.Session{UserId: c.userID}, c.perms)
		} else {
			appErr = model.MakePermissionErrorForUser(c.userID, c.perms)
		}
		names := make([]string, 0, len(c.perms))
		for _, p := range c.perms {
			names = append(names, p.Id)
		}
		out = append(out, permissionErrorCase{
			Name:        c.name,
			UserID:      c.userID,
			Permissions: names,
			Where:       appErr.Where,
			ID:          appErr.Id,
			Message:     appErr.Message,
			Detailed:    appErr.DetailedError,
			StatusCode:  appErr.StatusCode,
			ToJSON:      appErr.ToJSON(),
		})
	}
	return out
}
