package tui

import (
	"context"
	"io"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/app"
)

type programRunner interface {
	Run() (tea.Model, error)
}

var newProgram = func(model tea.Model, options ...tea.ProgramOption) programRunner {
	return tea.NewProgram(model, options...)
}

func Run(ctx context.Context, input io.Reader, output io.Writer, backend app.Backend) error {
	program := newProgram(
		NewModel(ctx, backend),
		tea.WithContext(rootContext(ctx)),
		tea.WithInput(input),
		tea.WithOutput(output),
		tea.WithoutSignalHandler(),
	)
	_, err := program.Run()
	return err
}
