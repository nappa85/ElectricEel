package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

const (
	keyLogFilePrefix = "phone-key-"
	keyLogKeepDays   = 7
	keyLogEnvDir     = "ELECTRIC_EEL_LOG_DIR"
)

var (
	keyLogMu   sync.Mutex
	keyLogDir  string
	keyLogDay  string
	keyLogFile *os.File
)

// initKeyLog creates (or reuses) the daily phone-key log under dir. Empty dir
// falls back to $ELECTRIC_EEL_LOG_DIR or ~/Documents/ElectricEel - the
// Documents folder is what Sailfish File Browser can open without hunting
// through Sailjail app data.
func initKeyLog(dir string) {
	if dir == "" {
		dir = defaultKeyLogDir()
	}
	keyLogMu.Lock()
	defer keyLogMu.Unlock()
	keyLogDir = dir
	_ = os.MkdirAll(dir, 0755)
	pruneKeyLogsLocked(dir, keyLogKeepDays, time.Now())
}

func defaultKeyLogDir() string {
	if d := os.Getenv(keyLogEnvDir); d != "" {
		return d
	}
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		home = os.Getenv("HOME")
	}
	return filepath.Join(home, "Documents", "ElectricEel")
}

func keyLogPath(dir string, day time.Time) string {
	return filepath.Join(dir, keyLogFilePrefix+day.Format("2006-01-02")+".log")
}

// keylog appends one human-readable line to today's log and to stderr (so
// journalctl still sees it if the Documents write fails). Safe from any
// goroutine. Never returns an error to callers: logging must not disturb
// the phone-key path.
func keylog(tag, format string, args ...interface{}) {
	now := time.Now()
	msg := fmt.Sprintf(format, args...)
	line := fmt.Sprintf("%s  %-10s  %s\n", now.Format("15:04:05.000"), tag, msg)

	keyLogMu.Lock()
	defer keyLogMu.Unlock()
	if f := openKeyLogLocked(now); f != nil {
		_, _ = f.WriteString(line)
	}
	_, _ = os.Stderr.WriteString("phone-key: " + line)
}

func openKeyLogLocked(now time.Time) *os.File {
	if keyLogDir == "" {
		return nil
	}
	day := now.Format("2006-01-02")
	if keyLogFile != nil && keyLogDay == day {
		return keyLogFile
	}
	if keyLogFile != nil {
		_ = keyLogFile.Close()
		keyLogFile = nil
	}
	path := keyLogPath(keyLogDir, now)
	created := false
	if _, err := os.Stat(path); err != nil {
		created = true
	}
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0644)
	if err != nil {
		return nil
	}
	if created {
		_, _ = fmt.Fprintf(f, "# ElectricEel phone-key log %s\n# tags: session presence connect auth link\n", day)
	}
	keyLogFile = f
	keyLogDay = day
	return f
}

func pruneKeyLogsLocked(dir string, keepDays int, now time.Time) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return
	}
	cutoff := now.AddDate(0, 0, -keepDays)
	for _, e := range entries {
		name := e.Name()
		if !strings.HasPrefix(name, keyLogFilePrefix) || !strings.HasSuffix(name, ".log") {
			continue
		}
		day := strings.TrimSuffix(strings.TrimPrefix(name, keyLogFilePrefix), ".log")
		t, err := time.Parse("2006-01-02", day)
		if err != nil {
			continue
		}
		if t.Before(cutoff) {
			_ = os.Remove(filepath.Join(dir, name))
		}
	}
}
