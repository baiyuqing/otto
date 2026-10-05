package main

import (
	"context"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"time"

	"github.com/baiyuqing/otto/desktop/internal/shell"
	"github.com/egoist/mygo"
)

const shutdownGrace = 10 * time.Second

type desktop struct {
	ctx       context.Context
	process   shell.Controller
	mu        sync.Mutex
	window    *mygo.Window
	serve     *url.URL
	binary    string
	env       []string
	statePath string
	picking   atomic.Bool
}

func main() {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	app := &desktop{ctx: ctx}
	startupDone := make(chan struct{})
	var stopping, cleaned atomic.Bool
	mygo.App.SetName("Otto")
	mygo.App.SetMenu(mygo.NewMenu([]*mygo.MenuItem{
		{Label: "Otto", Submenu: []*mygo.MenuItem{
			{Role: mygo.RoleAbout}, mygo.Separator(),
			{Role: mygo.RoleHide}, mygo.Separator(), {Role: mygo.RoleQuit},
		}},
		{Label: "File", Submenu: []*mygo.MenuItem{
			{Label: "Open Folder…", Accelerator: "CmdOrCtrl+O", Click: func(*mygo.MenuItem, *mygo.Window) { app.openFolder() }},
			mygo.Separator(), {Role: mygo.RoleClose},
		}},
		{Role: mygo.RoleEditMenu},
	}))
	// Keep the main loop alive while background cleanup runs: native window
	// operations and an in-flight startup dialog can still finish. A second
	// Quit after cleanup takes the normal MyGo exit path.
	mygo.App.OnBeforeQuit(func(e *mygo.QuitEvent) {
		if cleaned.Load() {
			return
		}
		e.PreventDefault()
		if stopping.CompareAndSwap(false, true) {
			cancel()
			go func() {
				app.process.Stop(shutdownGrace)
				<-startupDone
				cleaned.Store(true)
				mygo.App.Quit()
			}()
		}
	})
	mygo.App.OnWindowAllClosed(func() { mygo.App.Quit() })
	mygo.App.WhenReady(func() {
		go func() {
			defer close(startupDone)
			if err := app.start(); err != nil && ctx.Err() == nil {
				app.message("Otto", err.Error(), mygo.MessageError)
				mygo.App.Quit()
			}
		}()
	})
	if err := mygo.App.Run(); err != nil {
		cancel()
		app.process.Stop(shutdownGrace)
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func (d *desktop) start() error {
	home, err := os.UserHomeDir()
	if err != nil {
		return err
	}
	statePath, logPath := shell.Locations(home)
	probe, cancel := context.WithTimeout(d.ctx, 10*time.Second)
	env, captureErr := shell.CaptureEnvironment(probe)
	cancel()
	if d.ctx.Err() != nil {
		return d.ctx.Err()
	}
	if captureErr != nil {
		d.message("Environment capture", fmt.Sprintf("Otto could not capture your login shell's environment (%v). Using the app's own environment instead.", captureErr), mygo.MessageWarning)
	}
	resources, err := mygo.App.Path(mygo.PathResources)
	if err != nil {
		return err
	}
	binary := filepath.Join(resources, "bin", "otto")
	saved := shell.LoadState(statePath)
	workspace := saved.Workspace
	if info, err := os.Stat(workspace); err != nil || !info.IsDir() {
		workspace, err = d.pickFolder()
		if err != nil {
			return err
		}
		if workspace == "" {
			mygo.App.Quit()
			return nil
		}
	}
	if err := shell.Trust(d.ctx, binary, workspace, env); err != nil {
		return err
	}
	if err := shell.SaveState(statePath, shell.State{Workspace: workspace}); err != nil {
		return err
	}
	if d.ctx.Err() != nil {
		return d.ctx.Err()
	}
	child, err := shell.SpawnServe(binary, workspace, env, logPath)
	if err != nil {
		return err
	}
	if err := d.process.Install(child); err != nil {
		return err
	}
	wait, cancel := context.WithTimeout(d.ctx, 30*time.Second)
	serve, err := shell.WaitForServeURL(wait, child.Lines)
	cancel()
	if err != nil {
		message := shell.DescribeStartupFailure(child, err, logPath)
		d.process.Stop(shutdownGrace)
		return fmt.Errorf("%s", message)
	}
	if d.ctx.Err() != nil {
		return d.ctx.Err()
	}
	// Do not Bind services or add TrustedOrigins. The navigation bridge is
	// the only native action the existing web UI can request.
	window := mygo.NewWindow(mygo.WindowOptions{
		Title: "Otto", URL: serve.String(), Width: 1200, Height: 800, UseContentSize: true,
		Page: mygo.PageOptions{PreloadScript: shell.PreloadScript, DevTools: mygo.DevToolsDisabled},
	})
	window.Page().OnWillNavigate(func(e *mygo.NavigateEvent) {
		if shell.IsOpenFolderRequest(e.URL, serve) {
			e.PreventDefault()
			d.openFolder()
		}
	})
	d.mu.Lock()
	d.window, d.serve, d.binary, d.env, d.statePath = window, serve, binary, env, statePath
	d.mu.Unlock()
	return nil
}

func (d *desktop) pickFolder() (string, error) {
	d.mu.Lock()
	window := d.window
	d.mu.Unlock()
	paths, err := mygo.Dialog.Open(mygo.OpenDialogOptions{
		Parent: window, Title: "Choose a folder for Otto to work in", Directory: true,
	})
	if err != nil || len(paths) == 0 {
		return "", err
	}
	return paths[0], nil
}

// Serialize the menu and navigation requests so two pickers cannot race
// state writes. Blocking dialogs, trust and HTTP work stay off the UI loop.
func (d *desktop) openFolder() {
	if d.ctx.Err() != nil || !d.picking.CompareAndSwap(false, true) {
		return
	}
	go func() {
		defer d.picking.Store(false)
		d.mu.Lock()
		serve, binary, env, statePath, window := d.serve, d.binary, d.env, d.statePath, d.window
		d.mu.Unlock()
		if serve == nil {
			d.message("Otto", "Otto is still starting", mygo.MessageWarning)
			return
		}
		dir, err := d.pickFolder()
		if err == nil && dir == "" {
			return
		}
		if err == nil {
			err = shell.Trust(d.ctx, binary, dir, env)
		}
		if err == nil {
			err = shell.PostWorkspace(d.ctx, serve, dir)
		}
		if err == nil {
			err = shell.SaveState(statePath, shell.State{Workspace: dir})
		}
		if err != nil {
			if d.ctx.Err() == nil {
				d.message("Otto", err.Error(), mygo.MessageError)
			}
			return
		}
		window.Page().Reload()
	}()
}

func (d *desktop) message(title, detail string, kind mygo.MessageType) {
	d.mu.Lock()
	window := d.window
	d.mu.Unlock()
	_, _ = mygo.Dialog.Message(mygo.MessageOptions{Parent: window, Type: kind, Title: title, Message: title, Detail: detail})
}
