// Command hello is the Phase 0 spike plugin: the smallest real Mattermost plugin that exercises
// the handshake, Dispense, Implemented, a hook carrying structs both ways, and an error return.
package main

import (
	"errors"
	"fmt"
	"net/http"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/plugin"
)

type Hello struct {
	plugin.MattermostPlugin
}

// UserWillLogIn echoes what it decoded, so the launcher can check that its gob encoding of a
// partial `User` struct (a type defined by the Rust side with only two fields) was accepted.
func (p *Hello) UserWillLogIn(c *plugin.Context, user *model.User) string {
	return fmt.Sprintf("hello %s (%s) req=%s", user.Username, user.Id, c.RequestId)
}

// OnDeactivate returns an error so the reply carries a registered interface value.
func (p *Hello) OnDeactivate() error {
	return errors.New("hello: goodbye")
}

func (p *Hello) ServeHTTP(c *plugin.Context, w http.ResponseWriter, r *http.Request) {
	_, _ = fmt.Fprintf(w, "hello from %s", r.URL.Path)
}

func main() {
	plugin.ClientMain(&Hello{})
}
