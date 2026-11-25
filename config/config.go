// Package config loads ARTEX's TOML configuration and environment overrides.
package config

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/BurntSushi/toml"
)

const (
	filename       = "config.toml"
	legacyFilename = "config.json"
	defaultHost    = "127.0.0.1"
	defaultPort    = 5432
	defaultSSLMode = "disable"
)

// Database holds a PostgreSQL connection string or its individual components.
// The JSON tags support existing configuration files during migration.
type Database struct {
	DSN      string `toml:"dsn,omitempty" json:"dsn"`
	Host     string `toml:"host,omitempty" json:"host"`
	Port     int    `toml:"port,omitzero" json:"port"`
	User     string `toml:"user,omitempty" json:"user"`
	Password string `toml:"password,omitempty" json:"password"`
	DBName   string `toml:"db_name,omitempty" json:"dbname"`
	SSLMode  string `toml:"ssl_mode,omitempty" json:"sslmode"`
}

type skillConfig struct {
	Directory string `toml:"directory,omitempty"`
}

type fileConfig struct {
	Database Database    `toml:"database"`
	Skills   skillConfig `toml:"skills"`
}

// Settings is the resolved application configuration for one invocation.
type Settings struct {
	Path   string
	Exists bool

	file             fileConfig
	postgresOverride string
	skillDirOverride string
}

// BaseDir anchors runtime files such as config.toml and data/ next to the
// locally built executable, regardless of the current working directory.
//
// When launched via `go run`, the binary is throwaway: it sits either in a temp
// build dir (cache miss → fresh link) OR straight inside the Go build cache
// (cache hit → run from $GOCACHE/.../...-d). We detect both and fall back to the
// current working directory so dev artifacts (data/, transcripts) and config
// resolve against the project dir, not the throwaway binary's location.
func BaseDir() string {
	exe, err := os.Executable()
	if err != nil {
		return "."
	}
	dir := filepath.Dir(exe)
	if isGoRunDir(dir) {
		return "." // throwaway `go run` binary → use CWD
	}
	return dir
}

// isGoRunDir reports whether dir is where `go run` parked its executable: under
// the system temp dir (cache miss), or anywhere inside a Go build cache
// (…/go-build/…, the cache-hit case — NOT under os.TempDir(), which is why the
// old temp-only check failed intermittently). In both cases the binary is
// throwaway, so config/data must resolve against the CWD.
func isGoRunDir(dir string) bool {
	if tmp := os.TempDir(); tmp != "" {
		if rel, err := filepath.Rel(tmp, dir); err == nil && rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
			return true
		}
	}
	for _, seg := range strings.Split(filepath.ToSlash(dir), "/") {
		if seg == "go-build" {
			return true
		}
	}
	return false
}

// Read checks ARTEX_CONFIG first, then looks for config.toml in the working
// directory and beside the executable. A legacy file in a selected location
// requires migration so its settings are never silently skipped.
func Read() (Settings, error) {
	return ReadPath("")
}

// ReadPath uses the specified file when nonempty. An explicitly selected or
// discovered file must be readable and valid. With no file, env-only setup works.
func ReadPath(path string) (Settings, error) {
	if path == "" {
		path = strings.TrimSpace(os.Getenv("ARTEX_CONFIG"))
	}
	settings := Settings{
		postgresOverride: strings.TrimSpace(os.Getenv("ARTEX_PG_DSN")),
		skillDirOverride: strings.TrimSpace(os.Getenv("ARTEX_SKILL_DIR")),
	}
	if path != "" {
		return settings.load(path)
	}
	cwd, err := os.Getwd()
	if err != nil {
		return Settings{}, fmt.Errorf("locate working directory: %w", err)
	}
	for _, dir := range []string{cwd, BaseDir()} {
		for _, name := range []string{filename, legacyFilename} {
			candidate := filepath.Join(dir, name)
			if _, err := os.Lstat(candidate); errors.Is(err, os.ErrNotExist) {
				continue
			} else if err != nil {
				return Settings{}, err
			}
			return settings.load(candidate)
		}
	}
	settings.Path = filepath.Join(cwd, filename)
	return settings, nil
}

func readFile(path string) ([]byte, error) {
	info, err := os.Stat(path)
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() {
		return nil, fmt.Errorf("configuration %q is not a regular file", path)
	}
	return os.ReadFile(path)
}

func (s Settings) load(path string) (Settings, error) {
	data, err := readFile(path)
	if err != nil {
		return Settings{}, fmt.Errorf("read configuration: %w", err)
	}
	if strings.EqualFold(filepath.Ext(path), ".json") || bytes.HasPrefix(bytes.TrimSpace(data), []byte("{")) {
		return Settings{}, fmt.Errorf("legacy JSON configuration %q: convert it with artex -migrate-config and use the resulting TOML file", path)
	}
	s.file, err = decodeTOML(data)
	if err == nil {
		err = s.file.Database.validate()
	}
	if err != nil {
		return Settings{}, fmt.Errorf("configuration %q: %w", path, err)
	}
	s.Path = path
	s.Exists = true
	return s, nil
}

func decodeTOML(data []byte) (fileConfig, error) {
	var c fileConfig
	meta, err := toml.NewDecoder(bytes.NewReader(data)).Decode(&c)
	if err != nil {
		// Decoder error text may contain credentials from the malformed line.
		var parseErr toml.ParseError
		if errors.As(err, &parseErr) && parseErr.Position.Line > 0 {
			return fileConfig{}, fmt.Errorf("invalid TOML at line %d", parseErr.Position.Line)
		}
		return fileConfig{}, errors.New("invalid TOML or configuration field type")
	}
	if len(meta.Undecoded()) > 0 {
		return fileConfig{}, errors.New("unknown TOML key. See config.example.toml for supported keys")
	}
	return c, nil
}

func decodeLegacy(data []byte) (fileConfig, error) {
	var legacy struct {
		Database Database `json:"database"`
		SkillDir string   `json:"skill_dir"`
	}
	if !bytes.HasPrefix(bytes.TrimSpace(data), []byte("{")) || json.Unmarshal(data, &legacy) != nil {
		return fileConfig{}, errors.New("invalid legacy JSON configuration")
	}
	return fileConfig{Database: legacy.Database, Skills: skillConfig{Directory: legacy.SkillDir}}, nil
}

// MigrateLegacy converts a JSON file's settings to TOML. It retains the original
// and publishes the owner-only destination only if no entry already exists.
func MigrateLegacy(source, destination string) error {
	data, err := readFile(source)
	if err != nil {
		return fmt.Errorf("read legacy configuration: %w", err)
	}
	c, err := decodeLegacy(data)
	if err == nil {
		err = c.Database.validate()
	}
	if err != nil {
		return fmt.Errorf("configuration %q: %w", source, err)
	}

	tmp, err := os.CreateTemp(filepath.Dir(destination), ".artex-config-*")
	if err != nil {
		return fmt.Errorf("stage migrated configuration: %w", err)
	}
	defer os.Remove(tmp.Name())
	defer tmp.Close()
	if err := toml.NewEncoder(tmp).Encode(c); err != nil {
		// The encoder can include output data in some errors.
		return errors.New("write migrated TOML configuration")
	}
	if err := tmp.Sync(); err != nil {
		return fmt.Errorf("sync migrated configuration: %w", err)
	}
	if err := tmp.Close(); err != nil {
		return fmt.Errorf("close migrated configuration: %w", err)
	}
	if err := os.Link(tmp.Name(), destination); err != nil {
		return fmt.Errorf("publish migrated configuration: %w", err)
	}
	return nil
}

// SkillDir resolves the skill root without creating it. Relative paths remain
// relative to the working directory, including paths from a config file.
func (s Settings) SkillDir() string {
	if s.skillDirOverride != "" {
		return s.skillDirOverride
	}
	if dir := strings.TrimSpace(s.file.Skills.Directory); dir != "" {
		return dir
	}
	return filepath.Join(BaseDir(), "skills")
}

// PostgresDSN resolves the database settings using the same rules as Read.
func PostgresDSN() (dsn, source string, err error) {
	s, err := Read()
	if err != nil {
		return "", "", err
	}
	return s.PostgresDSN()
}

// PostgresDSN prefers ARTEX_PG_DSN, then the file's DSN or component fields.
// The source can be logged without exposing the returned connection string.
func (s Settings) PostgresDSN() (dsn, source string, err error) {
	dsn, err = s.Database().ConnectionString()
	if err != nil {
		return "", "", fmt.Errorf("未找到有效的数据库配置：请设置 ARTEX_PG_DSN 或配置 %q: %w", s.Path, err)
	}
	if s.postgresOverride != "" {
		return dsn, "环境变量 ARTEX_PG_DSN", nil
	}
	return dsn, "配置文件 " + s.Path, nil
}

// Database returns the effective PostgreSQL inputs, honoring ARTEX_PG_DSN.
func (s Settings) Database() Database {
	if s.postgresOverride != "" {
		return Database{DSN: s.postgresOverride}
	}
	return s.file.Database
}

func (d Database) validate() error {
	if strings.TrimSpace(d.DSN) != "" {
		return nil // A DSN replaces the component fields. pgx validates its syntax.
	}
	if d.Port < 0 || d.Port > 65535 {
		return errors.New("database.port must be between 1 and 65535, or 0 for the default")
	}
	switch d.SSLMode {
	case "", "disable", "allow", "prefer", "require", "verify-ca", "verify-full":
		return nil
	default:
		return errors.New("invalid database.ssl_mode")
	}
}

// ConnectionString uses an explicit DSN verbatim after trimming surrounding
// whitespace. With components, host, port, and ssl_mode have defaults.
func (d Database) ConnectionString() (string, error) {
	if dsn := strings.TrimSpace(d.DSN); dsn != "" {
		return dsn, nil
	}
	if err := d.validate(); err != nil {
		return "", err
	}
	if d.Host == "" && d.DBName == "" && d.User == "" {
		return "", errors.New("set database.dsn or database connection fields")
	}
	if d.Host == "" {
		d.Host = defaultHost
	}
	if d.Port == 0 {
		d.Port = defaultPort
	}
	if d.SSLMode == "" {
		d.SSLMode = defaultSSLMode
	}
	// JoinHostPort adds IPv6 brackets itself. Accept already bracketed hosts too.
	host := strings.TrimSuffix(strings.TrimPrefix(d.Host, "["), "]")
	u := url.URL{
		Scheme: "postgres",
		Host:   net.JoinHostPort(host, strconv.Itoa(d.Port)),
		Path:   "/" + d.DBName,
	}
	if d.User != "" {
		if d.Password != "" {
			u.User = url.UserPassword(d.User, d.Password)
		} else {
			u.User = url.User(d.User)
		}
	}
	u.RawQuery = url.Values{"sslmode": {d.SSLMode}}.Encode()
	return u.String(), nil
}

// Redact returns address and database information without DSN credentials or
// query parameters. Non-URL connection strings are hidden entirely.
func Redact(dsn string) string {
	u, err := url.Parse(dsn)
	if err != nil || (u.Scheme != "postgres" && u.Scheme != "postgresql") || u.Opaque != "" {
		return "[redacted PostgreSQL connection string]"
	}
	return (&url.URL{Scheme: u.Scheme, Host: u.Host, Path: u.Path}).String()
}
