package main

// Behavioural oracle for model/file_info.go, written to fixtures/behaviour_file_info.json.
//
// Four things in this file need Go's own answer rather than a reading:
//
//  1. **`MiniPreview *[]byte` marshals as base64, not as an array of numbers.** Go's
//     encoding/json special-cases `[]byte`; serde_json does not, and would emit `[1,2,3]` where
//     Go emits `"AQID"`. There are also three distinguishable nil-ish states — nil pointer,
//     pointer to a nil slice, pointer to an empty slice — and Go collapses two of them. The
//     `wire` section records the exact bytes for each.
//
//  2. **`SanitizeFilename` NFC-normalizes**, which needs a Unicode normalization table on the
//     Rust side. The corpus includes decomposed input so the dependency is justified by a
//     failing case rather than by assumption.
//
//  3. **`NewInfo` calls `mime.TypeByExtension`, which reads the host's mime.types files.** That
//     makes it environment-dependent and not portable in principle; the corpus records what this
//     host said so the divergence is visible rather than assumed away. See D-030.
//
//  4. **`IsValid` requires a non-empty `Path`, and `Path` carries `json:"-"`.** So a FileInfo
//     deserialized straight off the wire is *always* invalid. Pinned, because it looks like a
//     bug in the port otherwise.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeFileInfoBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":                 fileInfoConstants(),
		"wire":                      fileInfoWireAll(),
		"is_valid":                  fileInfoIsValidAll(),
		"is_valid_filename":         isValidFilenameAll(),
		"sanitize_filename":         sanitizeFilenameAll(),
		"pre_save":                  fileInfoPreSaveAll(),
		"is_image":                  fileInfoMimeAll(),
		"new_info":                  newInfoAll(),
		"get_etag_for_file_infos":   fileInfoEtagAll(),
		"make_content_inaccessible": makeContentInaccessibleAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_file_info.json"), append(blob, '\n'), 0o644)
}

func fileInfoConstants() map[string]any {
	return map[string]any{
		"sort_by_created":         model.FileinfoSortByCreated,
		"sort_by_size":            model.FileinfoSortBySize,
		"max_filename_length":     model.MaxFilenameLength,
		"download_type_file":      string(model.FileDownloadTypeFile),
		"download_type_thumbnail": string(model.FileDownloadTypeThumbnail),
		"download_type_preview":   string(model.FileDownloadTypePreview),
		"download_type_public":    string(model.FileDownloadTypePublic),
		// Borrowed from channel_bookmark.go; recorded so the borrow cannot drift silently.
		"bookmark_file_owner": model.BookmarkFileOwner,
	}
}

// --- the wire format ------------------------------------------------------------------

type wireCase struct {
	Name string `json:"name"`
	JSON string `json:"json"`
}

// fileInfoWireAll pins the exact bytes for the fields whose Go type does not map to a serde
// default: the base64 `*[]byte`, the `*string`, and the four omitempty fields.
func fileInfoWireAll() []wireCase {
	empty := []byte{}
	data := []byte{0x01, 0x02, 0x03}
	var nilSlice []byte
	remote := "cluster-a"
	emptyRemote := ""

	cases := []struct {
		name string
		fi   model.FileInfo
	}{
		{"zero", model.FileInfo{}},
		{"mini_preview_nil_pointer", model.FileInfo{MiniPreview: nil}},
		{"mini_preview_pointer_to_nil_slice", model.FileInfo{MiniPreview: &nilSlice}},
		{"mini_preview_pointer_to_empty_slice", model.FileInfo{MiniPreview: &empty}},
		{"mini_preview_pointer_to_data", model.FileInfo{MiniPreview: &data}},
		{"remote_id_nil", model.FileInfo{RemoteId: nil}},
		{"remote_id_empty", model.FileInfo{RemoteId: &emptyRemote}},
		{"remote_id_set", model.FileInfo{RemoteId: &remote}},
		// The omitempty fields, at and away from their zero values.
		{"omitempty_all_zero", model.FileInfo{}},
		{"omitempty_all_set", model.FileInfo{
			PostId: idB, Width: 640, Height: 480, HasPreviewImage: true,
		}},
		// The json:"-" fields must not appear whatever they hold.
		{"hidden_fields_populated", model.FileInfo{
			Path: "/a/b", ThumbnailPath: "/a/t", PreviewPath: "/a/p", Content: "extracted text",
		}},
		{"archived", model.FileInfo{Archived: true}},
	}

	res := make([]wireCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(&c.fi)
		if err != nil {
			panic(err)
		}
		res = append(res, wireCase{Name: c.name, JSON: string(blob)})
	}
	return res
}

// --- IsValid ---------------------------------------------------------------------------

type fileInfoValidCase struct {
	Name     string          `json:"name"`
	FileInfo json.RawMessage `json:"file_info"`
	// Path carries json:"-", so it cannot be recovered from the marshalled form.
	Path     string `json:"path"`
	ErrorID  string `json:"error_id"`
	Detailed string `json:"detailed"`
}

func fileInfoIsValidAll() []fileInfoValidCase {
	type mut struct {
		name string
		fn   func(fi *model.FileInfo)
	}
	muts := []mut{
		{"valid", func(fi *model.FileInfo) {}},

		{"id_empty", func(fi *model.FileInfo) { fi.Id = "" }},
		{"id_short", func(fi *model.FileInfo) { fi.Id = repeat("a", 25) }},

		// CreatorId accepts two magic strings as well as a real id.
		{"creator_id_empty", func(fi *model.FileInfo) { fi.CreatorId = "" }},
		{"creator_id_nouser", func(fi *model.FileInfo) { fi.CreatorId = "nouser" }},
		{"creator_id_bookmark", func(fi *model.FileInfo) { fi.CreatorId = model.BookmarkFileOwner }},
		{"creator_id_nonsense", func(fi *model.FileInfo) { fi.CreatorId = "nope" }},
		{"creator_id_NoUser_wrong_case", func(fi *model.FileInfo) { fi.CreatorId = "NoUser" }},

		// PostId is optional, but must be a real id when present.
		{"post_id_empty", func(fi *model.FileInfo) { fi.PostId = "" }},
		{"post_id_nonsense", func(fi *model.FileInfo) { fi.PostId = "nope" }},

		{"create_at_zero", func(fi *model.FileInfo) { fi.CreateAt = 0 }},
		{"update_at_zero", func(fi *model.FileInfo) { fi.UpdateAt = 0 }},

		// Path is required — and is never on the wire.
		{"path_empty", func(fi *model.FileInfo) { fi.Path = "" }},

		// Name is optional, but must pass IsValidFilename when present.
		{"name_empty", func(fi *model.FileInfo) { fi.Name = "" }},
		{"name_dot", func(fi *model.FileInfo) { fi.Name = "." }},
		{"name_dotdot", func(fi *model.FileInfo) { fi.Name = ".." }},
		{"name_slash", func(fi *model.FileInfo) { fi.Name = "a/b.txt" }},
		{"name_backslash", func(fi *model.FileInfo) { fi.Name = `a\b.txt` }},
		{"name_control_char", func(fi *model.FileInfo) { fi.Name = "a\x01b.txt" }},
		{"name_256", func(fi *model.FileInfo) { fi.Name = repeat("a", 256) }},
		{"name_257", func(fi *model.FileInfo) { fi.Name = repeat("a", 257) }},
		{"name_256_runes_multibyte", func(fi *model.FileInfo) { fi.Name = repeat("é", 256) }},
		{"name_257_runes_multibyte", func(fi *model.FileInfo) { fi.Name = repeat("é", 257) }},

		// Neither of these is checked.
		{"channel_id_empty", func(fi *model.FileInfo) { fi.ChannelId = "" }},
		{"delete_at_set", func(fi *model.FileInfo) { fi.DeleteAt = 1700000000000 }},
	}

	var res []fileInfoValidCase
	for _, m := range muts {
		remote := "cluster-a"
		fi := &model.FileInfo{
			Id:        idA,
			CreatorId: idB,
			PostId:    idC,
			ChannelId: idC,
			CreateAt:  1700000000000,
			UpdateAt:  1700000000000,
			Path:      "20231114/teams/x/channels/y/users/z/file.txt",
			Name:      "file.txt",
			Extension: "txt",
			Size:      1024,
			MimeType:  "text/plain",
			RemoteId:  &remote,
		}
		m.fn(fi)

		blob, err := json.Marshal(fi)
		if err != nil {
			panic(err)
		}
		c := fileInfoValidCase{Name: m.name, FileInfo: blob, Path: fi.Path}
		if appErr := fi.IsValid(); appErr != nil {
			c.ErrorID = appErr.Id
			c.Detailed = appErr.DetailedError
		}
		res = append(res, c)
	}
	return res
}

// --- IsValidFilename / SanitizeFilename ---------------------------------------------------

// filenameCorpus is shared by both functions so their disagreements are visible side by side —
// SanitizeFilename can return something IsValidFilename still rejects, and vice versa.
var filenameCorpus = []string{
	"", ".", "..", "...", "file.txt", "no-extension", ".hidden", "a.b.c",
	"/", "//", "/a", "a/", "a/b", "a/b/c.txt", `\`, `a\b`, `a\b\c.txt`, `\\server\share\f.txt`,
	"with space.txt", "  leading.txt", "trailing.txt  ",
	"\x00null", "a\x01b", "a\x1fb", "a\x7fb", "\x7f", "a\tb", "a\nb",
	"unicode-é.txt", "日本語.txt", "\U0001F600.png",
	// NFC vs NFD: "é" as one codepoint, and as "e" + combining acute.
	"é.txt", "é.txt",
	repeat("a", 255), repeat("a", 256), repeat("a", 257), repeat("a", 300),
	repeat("é", 256), repeat("é", 257),
	// A decomposed name that is 257 runes before normalization and fewer after.
	repeat("é", 200) + ".txt",
	"a/" + repeat("b", 300),
	"./file.txt", "../file.txt", "dir/../file.txt",
}

func isValidFilenameAll() map[string]bool {
	res := make(map[string]bool, len(filenameCorpus))
	for _, n := range filenameCorpus {
		res[n] = model.IsValidFilename(n)
	}
	return res
}

type sanitizeCase struct {
	In  string `json:"in"`
	Out string `json:"out"`
	// Whether the sanitized form then passes IsValidFilename — sanitizing is not validating.
	OutValid bool `json:"out_valid"`
}

func sanitizeFilenameAll() []sanitizeCase {
	res := make([]sanitizeCase, 0, len(filenameCorpus))
	for _, n := range filenameCorpus {
		out := model.SanitizeFilename(n)
		res = append(res, sanitizeCase{In: n, Out: out, OutValid: model.IsValidFilename(out)})
	}
	return res
}

// --- PreSave ------------------------------------------------------------------------------

type fileInfoPreSaveCase struct {
	Name        string `json:"name"`
	InID        string `json:"in_id"`
	InCreateAt  int64  `json:"in_create_at"`
	InUpdateAt  int64  `json:"in_update_at"`
	InRemoteNil bool   `json:"in_remote_nil"`

	IDPreserved       bool  `json:"id_preserved"`
	IDGenerated       bool  `json:"id_generated"`
	CreateAtPreserved bool  `json:"create_at_preserved"`
	OutUpdateAt       int64 `json:"out_update_at"`
	UpdateAtRaised    bool  `json:"update_at_raised"`
	OutRemoteNil      bool  `json:"out_remote_nil"`
}

func fileInfoPreSaveAll() []fileInfoPreSaveCase {
	cases := []struct {
		name               string
		id                 string
		createAt, updateAt int64
		remoteNil          bool
	}{
		{"all_zero", "", 0, 0, true},
		{"id_kept", idA, 1700000000000, 1700000000000, true},
		{"update_at_behind_create_at", idA, 1700000000000, 1600000000000, true},
		{"update_at_ahead_of_create_at", idA, 1700000000000, 1800000000000, true},
		{"update_at_zero_create_at_set", idA, 1700000000000, 0, true},
		{"remote_set", idA, 1700000000000, 1700000000000, false},
	}

	var res []fileInfoPreSaveCase
	for _, c := range cases {
		fi := &model.FileInfo{Id: c.id, CreateAt: c.createAt, UpdateAt: c.updateAt}
		if !c.remoteNil {
			remote := "cluster-a"
			fi.RemoteId = &remote
		}
		fi.PreSave()

		res = append(res, fileInfoPreSaveCase{
			Name:              c.name,
			InID:              c.id,
			InCreateAt:        c.createAt,
			InUpdateAt:        c.updateAt,
			InRemoteNil:       c.remoteNil,
			IDPreserved:       c.id != "" && fi.Id == c.id,
			IDGenerated:       c.id == "" && len(fi.Id) == 26,
			CreateAtPreserved: c.createAt != 0 && fi.CreateAt == c.createAt,
			OutUpdateAt:       fi.UpdateAt,
			UpdateAtRaised:    fi.UpdateAt != c.updateAt,
			OutRemoteNil:      fi.RemoteId == nil,
		})
	}
	return res
}

// --- IsImage / IsSvg / NewInfo -------------------------------------------------------------

type mimeCase struct {
	MimeType string `json:"mime_type"`
	IsImage  bool   `json:"is_image"`
	IsSvg    bool   `json:"is_svg"`
}

func fileInfoMimeAll() []mimeCase {
	types := []string{
		"", "image/png", "image/jpeg", "image/svg+xml", "IMAGE/PNG", "text/plain",
		"application/pdf", "image", "images/png", "imagex", "x-image/png",
		"image/svg+xml; charset=utf-8", " image/png",
	}
	res := make([]mimeCase, 0, len(types))
	for _, t := range types {
		fi := &model.FileInfo{MimeType: t}
		res = append(res, mimeCase{MimeType: t, IsImage: fi.IsImage(), IsSvg: fi.IsSvg()})
	}
	return res
}

type newInfoCase struct {
	In        string `json:"in"`
	Name      string `json:"name"`
	Extension string `json:"extension"`
	MimeType  string `json:"mime_type"`
}

// newInfoAll records what this host's mime table said. The extension split is portable; the
// mime lookup is not — see the file header and D-030.
func newInfoAll() []newInfoCase {
	names := []string{
		"", "file.txt", "file.PNG", "file.png", "photo.jpeg", "a.tar.gz",
		"noextension", ".hidden", "file.", "file.unknownext",
		"dir/file.png", "file.SVG", "doc.pdf", "page.html", "data.json",
		"script.js", "style.css", "image.webp", "image.gif", "movie.mp4",
	}
	res := make([]newInfoCase, 0, len(names))
	for _, n := range names {
		info := model.NewInfo(n)
		res = append(res, newInfoCase{
			In: n, Name: info.Name, Extension: info.Extension, MimeType: info.MimeType,
		})
	}
	return res
}

// --- GetEtagForFileInfos ---------------------------------------------------------------------

type fileInfoEtagCase struct {
	Name  string            `json:"name"`
	Infos []json.RawMessage `json:"infos"`
	Out   string            `json:"out"`
}

func fileInfoEtagAll() []fileInfoEtagCase {
	mk := func(postID string, updateAt int64) *model.FileInfo {
		return &model.FileInfo{Id: model.NewId(), PostId: postID, UpdateAt: updateAt}
	}

	lists := []struct {
		name  string
		infos []*model.FileInfo
	}{
		{"nil", nil},
		{"empty", []*model.FileInfo{}},
		{"single", []*model.FileInfo{mk(idA, 1700000000000)}},
		// The etag takes infos[0].PostId but the *max* UpdateAt, which can come from any
		// element — the same shape of trap as the channel-list etags.
		{"max_from_later_element", []*model.FileInfo{mk(idA, 100), mk(idB, 900), mk(idC, 500)}},
		{"max_from_first_element", []*model.FileInfo{mk(idA, 900), mk(idB, 100)}},
		{"all_zero_update_at", []*model.FileInfo{mk(idA, 0), mk(idB, 0)}},
		{"negative_update_at", []*model.FileInfo{mk(idA, -5), mk(idB, -1)}},
		{"empty_post_id", []*model.FileInfo{mk("", 700)}},
	}

	var res []fileInfoEtagCase
	for _, l := range lists {
		infos := make([]json.RawMessage, 0, len(l.infos))
		for _, fi := range l.infos {
			blob, err := json.Marshal(fi)
			if err != nil {
				panic(err)
			}
			infos = append(infos, blob)
		}
		res = append(res, fileInfoEtagCase{
			Name:  l.name,
			Infos: infos,
			Out:   model.GetEtagForFileInfos(l.infos),
		})
	}
	return res
}

// --- MakeContentInaccessible --------------------------------------------------------------

func makeContentInaccessibleAll() map[string]any {
	preview := []byte{1, 2, 3}
	fi := &model.FileInfo{
		Id:              idA,
		CreatorId:       idB,
		Archived:        false,
		Content:         "extracted text",
		HasPreviewImage: true,
		MiniPreview:     &preview,
		Path:            "/a/b",
		PreviewPath:     "/a/p",
		ThumbnailPath:   "/a/t",
		Name:            "file.txt",
		Size:            1024,
	}
	fi.MakeContentInaccessible()

	blob, err := json.Marshal(fi)
	if err != nil {
		panic(err)
	}

	return map[string]any{
		"archived":          fi.Archived,
		"content":           fi.Content,
		"has_preview_image": fi.HasPreviewImage,
		"mini_preview_nil":  fi.MiniPreview == nil,
		"path":              fi.Path,
		"preview_path":      fi.PreviewPath,
		"thumbnail_path":    fi.ThumbnailPath,
		// Untouched fields, to show what it does *not* clear.
		"name": fi.Name,
		"size": fi.Size,
		"json": string(blob),
	}
}
