package tui

import (
	"context"
	"errors"
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
	finalModel, programErr := program.Run()
	fatalErr := fatalErrorFromModel(finalModel)
	if fatalErr != nil && programErr != nil {
		return errors.Join(fatalErr, programErr)
	}
	if fatalErr != nil {
		return fatalErr
	}
	return programErr
}

func fatalErrorFromModel(finalModel tea.Model) error {
	switch final := finalModel.(type) {
	case Model:
		return final.fatalErr
	case *Model:
		if final != nil {
			return final.fatalErr
		}
	}
	return nil
}
