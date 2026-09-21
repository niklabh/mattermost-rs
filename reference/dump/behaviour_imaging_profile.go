package main

// The **profile-picture** stage of the imaging oracle: `users.createProfileImage`
// (channels/app/users/profile_picture.go:104) and the freetype-go rasterisation underneath it —
// `truetype.NewFace`, `font.Drawer.DrawString`, `raster`'s scan converter and the `image/draw`
// composites, over `fonts/nunito-bold.ttf`.
//
// It lives with the imaging stages because it shares their framework (`describe`, `encoded`, the
// per-line marshaller) and their regeneration switch (`go run . -only=imaging`), not because it is
// part of the image pipeline.
//
// **Stub** — filled in by the rasteriser port (docs/TECH_DEBT.md D-204, D-411 item 2).

func imagingProfilePictureStage() (map[string]any, error) {
	return map[string]any{"cases": []any{}}, nil
}
