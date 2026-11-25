package config

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func writeFixture(t *testing.T, path, value string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(value), 0o600); err != nil {
		t.Fatal(err)
	}
}

func isolatedConfigDir(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	t.Chdir(dir)
	t.Setenv("ARTEX_CONFIG", "")
	t.Setenv("ARTEX_PG_DSN", "")
	t.Setenv("ARTEX_SKILL_DIR", "")
	return dir
}

func TestTOMLConfigurationPrecedence(t *testing.T) {
	dir := isolatedConfigDir(t)
	legacyPath := filepath.Join(dir, "config.json")
	tomlPath := filepath.Join(dir, "config.toml")
	writeFixture(t, legacyPath, `{"database":{"dsn":"postgres://legacy/db"},"skill_dir":"legacy-skills"}`)
	writeFixture(t, tomlPath, `[database]
host = "10.1.2.3"
port = 6000
user = "u"
password = "p"
db_name = "d"
ssl_mode = "require"

[skills]
directory = "relative/skills"
`)

	s, err := Read()
	if err != nil || s.Path != tomlPath || !s.Exists {
		t.Fatalf("selected path %q, exists %t, err %v", s.Path, s.Exists, err)
	}
	if got, _, err := s.PostgresDSN(); err != nil || got != "postgres://u:p@10.1.2.3:6000/d?sslmode=require" {
		t.Fatal("did not build DSN from TOML components")
	}
	if got := s.SkillDir(); got != "relative/skills" {
		t.Fatalf("skill directory = %q", got)
	}
	if _, err := os.Stat("relative"); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("config resolution created the skill directory")
	}

	customPath := filepath.Join(dir, "custom.toml")
	writeFixture(t, customPath, "[skills]\ndirectory = 'custom-skills'\n")
	t.Setenv("ARTEX_CONFIG", customPath)
	custom, err := Read()
	if err != nil || custom.Path != customPath || custom.SkillDir() != "custom-skills" {
		t.Fatal("ARTEX_CONFIG did not select the custom TOML file")
	}
	if explicit, err := ReadPath(tomlPath); err != nil || explicit.Path != tomlPath {
		t.Fatal("explicit path did not win over ARTEX_CONFIG")
	}
	t.Setenv("ARTEX_PG_DSN", "postgres://envwins/x")
	t.Setenv("ARTEX_SKILL_DIR", "/env/skills")
	overrides, err := Read()
	if err != nil || overrides.SkillDir() != "/env/skills" {
		t.Fatal("environment should override the configured skill path")
	}
	if got, _, err := overrides.PostgresDSN(); err != nil || got != "postgres://envwins/x" {
		t.Fatal("environment should override the configured database")
	}
}

func TestMissingAndLegacyFiles(t *testing.T) {
	dir := isolatedConfigDir(t)
	s, err := Read()
	if err != nil || s.Exists {
		t.Fatalf("default config absent: exists %t, err %v", s.Exists, err)
	}
	if _, _, err := s.PostgresDSN(); err == nil {
		t.Fatal("database must be explicitly configured")
	}
	if s.SkillDir() != filepath.Join(BaseDir(), "skills") {
		t.Fatal("incorrect default skills directory")
	}
	t.Setenv("ARTEX_PG_DSN", "dbname=env_database user=env_user")
	if got, _, err := PostgresDSN(); err != nil || got != "dbname=env_database user=env_user" {
		t.Fatal("missing default configuration should permit environment-only setup")
	}
	t.Setenv("ARTEX_CONFIG", filepath.Join(dir, "missing.toml"))
	if _, err := Read(); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("missing explicit configuration did not fail: %v", err)
	}

	t.Setenv("ARTEX_CONFIG", "")
	t.Setenv("ARTEX_PG_DSN", "")
	legacy := `{"database":{"dsn":"postgres://legacy/db"},"skill_dir":"legacy-skills"}`
	writeFixture(t, "config.json", legacy)
	if _, err := Read(); err == nil || !strings.Contains(err.Error(), "-migrate-config") {
		t.Fatal("a discovered legacy config.json should require migration")
	}
	writeFixture(t, "config.toml", "[skills]\ndirectory = 'current-skills'\n")
	writeFixture(t, "custom.settings", legacy)
	for _, path := range []string{"config.json", "custom.settings"} {
		t.Setenv("ARTEX_CONFIG", path)
		if _, err := Read(); err == nil || !strings.Contains(err.Error(), "-migrate-config") {
			t.Fatalf("explicit legacy path %q should require migration", path)
		}
	}
}

func TestInvalidConfigurationDoesNotFallBackOrExposeValues(t *testing.T) {
	isolatedConfigDir(t)
	writeFixture(t, "config.json", `{"database":{"dsn":"postgres://legacy/db"}}`)
	for _, value := range []string{
		"[database]\npassword = \"synthetic_secret\n",
		"[database]\npassword = [\"synthetic_secret\"]\n",
		"[database]\nsynthetic_secret = 'unknown-field'\n",
		"[database]\nssl_mode = 'synthetic_secret'\n",
		"[database]\nport = -1\n",
		"[database]\nport = 65536\n",
		`{"database":{"dsn":"postgres://user:synthetic_secret@host/db"}}`,
	} {
		writeFixture(t, "config.toml", value)
		if _, err := Read(); err == nil || strings.Contains(err.Error(), "synthetic_secret") {
			t.Fatal("invalid config was accepted or error disclosed a configuration value")
		}
	}
}

func TestDatabaseConnectionDefaultsAndIPv6(t *testing.T) {
	for _, host := range []string{"::1", "[::1]"} {
		d := Database{Host: host, User: "operator", Password: "p@ss / 测试", DBName: "lab"}
		const want = "postgres://operator:p%40ss%20%2F%20%E6%B5%8B%E8%AF%95@[::1]:5432/lab?sslmode=disable"
		if got, err := d.ConnectionString(); err != nil || got != want {
			t.Fatalf("could not assemble escaped IPv6 DSN for host %q, err %v", host, err)
		}
	}
	d := Database{DSN: `host=/run/postgresql user=artex dbname=test password='a b'`, Port: -1}
	if got, err := d.ConnectionString(); err != nil || got != d.DSN {
		t.Fatal("an explicit connection string should replace all component fields")
	}
}

func TestMigrateLegacyPreservesSettingsAndRefusesOverwrite(t *testing.T) {
	dir := isolatedConfigDir(t)
	source, target := filepath.Join(dir, "old.json"), filepath.Join(dir, "config.toml")
	password := "synthetic \"quoted\" and \\ newline\n测试"
	legacy, err := json.Marshal(map[string]any{
		"database": map[string]any{
			"host": "db.internal", "port": 5439, "user": "operator", "password": password,
			"dbname": "artex-data", "sslmode": "verify-full",
		},
		"skill_dir": "custom/skills",
		"_comment":  "documentation from the previous example",
	})
	if err != nil {
		t.Fatal(err)
	}
	writeFixture(t, source, string(legacy))
	t.Setenv("ARTEX_PG_DSN", "postgres://override/db")
	t.Setenv("ARTEX_SKILL_DIR", "override-skills")
	if err := MigrateLegacy(source, target); err != nil {
		t.Fatal(err)
	}
	t.Setenv("ARTEX_PG_DSN", "")
	t.Setenv("ARTEX_SKILL_DIR", "")
	after, err := Read()
	if err != nil || after.Path != target {
		t.Fatal("could not read the migrated TOML file")
	}
	want := Database{Host: "db.internal", Port: 5439, User: "operator", Password: password,
		DBName: "artex-data", SSLMode: "verify-full"}
	if after.Database() != want || after.SkillDir() != "custom/skills" {
		t.Fatal("migration changed a database field or skill directory")
	}
	if info, err := os.Stat(target); err != nil || info.Mode().Perm() != 0o600 {
		t.Fatal("migrated configuration must be owner-only")
	}
	if contents, err := os.ReadFile(source); err != nil || string(contents) != string(legacy) {
		t.Fatal("migration modified the source JSON")
	}

	migrated, err := os.ReadFile(target)
	if err != nil {
		t.Fatal(err)
	}
	if err := MigrateLegacy(source, target); !errors.Is(err, os.ErrExist) {
		t.Fatalf("existing destination was not protected: %v", err)
	}
	if contents, err := os.ReadFile(target); err != nil || string(contents) != string(migrated) {
		t.Fatal("migration modified an existing destination")
	}
}

func TestRedactHidesDSNCredentials(t *testing.T) {
	for _, dsn := range []string{
		"postgres://user:synthetic_secret@localhost/db?password=synthetic_secret",
		"postgres://user:synthetic_secret@localhost/%[",
		"user=operator password='synthetic_secret' dbname=test",
	} {
		if got := Redact(dsn); strings.Contains(got, "synthetic_secret") {
			t.Fatal("redacted DSN contains a credential")
		}
	}
}
