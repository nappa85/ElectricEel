package main

import (
	"bytes"
	"context"
	"fmt"
	"io"
)

type outputContextKey struct{}

type commandWriters struct {
	stdout io.Writer
	stderr io.Writer
}

func commandOutput(ctx context.Context) commandWriters {
	if writers, ok := ctx.Value(outputContextKey{}).(commandWriters); ok {
		return writers
	}
	// No per-command writers (background diagnostics, tests): discard so
	// logs stay clean. stdout is the parent protocol's log stream, never a
	// command-output channel.
	return commandWriters{stdout: io.Discard, stderr: io.Discard}
}

func writeErr(ctx context.Context, format string, args ...interface{}) {
	fmt.Fprintf(commandOutput(ctx).stderr, format+"\n", args...)
}

// Only this command's handlers write into these buffers. Diagnostics and other
// commands keep their own writers; process-global stdout/stderr never change.
func captureOutput(ctx context.Context, f func(context.Context)) (stdout, stderr string) {
	var out, errOut bytes.Buffer
	f(context.WithValue(ctx, outputContextKey{}, commandWriters{&out, &errOut}))
	return out.String(), errOut.String()
}
