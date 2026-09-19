# Rune — Spezifikation

> Status: Draft / Living Document
> Zweck: Grundlage für Coding-Agents und Contributors, um Scope, Architektur und Entscheidungen von Rune zu verstehen, bevor Code geschrieben wird.

## 1. Elevator Pitch

Rune ist ein alternativer Package Manager / Toolkit für Rust-Projekte, der API- und Manifest-kompatibel zu Cargo ist, aber signifikant schneller bei Dependency Resolution, Downloads und Caching arbeitet, und Standard-Tooling (Audit, Coverage, Tree, Why, ...) nativ mitbringt statt es auf externe Subcommands auszulagern.

Grober Vergleichspunkt: was Bun für den JS-Ökosystem-Wildwuchs war, soll Rune für die fragmentierte Cargo-Subcommand-Landschaft sein — mit dem Unterschied, dass Rune sich bewusst an Cargo andockt statt es zu ersetzen.

## 2. Problem Statement

Cargo ist der De-facto-Standard und offiziell mit Rust ausgeliefert. Trotzdem gibt es wiederkehrende Schmerzpunkte:

- **Dependency Resolution & Downloads**: trotz Sparse-Registry-Protokoll spürbar langsam bei großen Graphen, kein aggressives Parallelisieren von Downloads out of the box.
- **Fehlendes Standard-Tooling**: `audit`, `outdated`, `deny`, `why`, `coverage`, `machete` existieren nur als separat zu installierende Subcommands mit inkonsistenter UX, keine einheitliche Namensgebung, keine gemeinsame Konfiguration.
- **Monorepo-Support**: Workspace-Handling ist im Vergleich zu Bazel/Buck2/Pants/Turborepo rudimentär (kein Task-Graph, kein selektives Caching pro Package, keine Remote-Caches).
- **Kein Content-Addressable Store**: Wiederverwendung von Build-Artefakten über Projekte/Maschinen hinweg ist nicht eingebaut (sccache existiert, aber als externes Add-on).
- **Kein First-Class CI-Support**: keine eingebauten Primitives für CI-optimierte Caches, Fingerprinting, Remote Execution.

## 3. Nicht-Ziele (bewusst außerhalb des Scopes, zumindest für v1)

- Rune ist **kein Ersatz für rustc**. Reine Compile-Zeit (Codegen, LLVM-Backend, Monomorphisierung) liegt außerhalb des Einflussbereichs eines Package Managers. Rune optimiert den Build-**Orchestrierungs**-Layer (Scheduling, Caching, Parallelisierung), nicht den Compiler selbst.
- Rune ist **kein rustup-Ersatz** in v1. Toolchain-Pinning (Node/Bun/Deno-artig) ist ein separates Problemfeld und wird explizit auf eine spätere Phase verschoben, um Scope-Explosion zu vermeiden.
- Rune ist **kein General-Purpose-Build-System** wie Bazel/Buck2. Kein Anspruch auf Sprachagnostizismus.

## 4. Zielgruppe / Use Cases

- Einzelentwickler, die schnellere lokale Iteration wollen (Resolution, Downloads, Caching).
- Teams mit großen Cargo-Workspaces (Monorepos), die selektive Builds/Tests und Remote-Caching brauchen.
- CI-Pipelines, die von einem eingebauten, deterministischen Cache-Modell profitieren.

## 5. Kompatibilitätsanforderungen

- **Manifest-Kompatibilität**: Rune muss existierende `Cargo.toml`-Dateien ohne Änderung lesen und korrekt auflösen können (inkl. Workspace-Inheritance, `[patch]`, `dep:`-Feature-Syntax, target-spezifische Dependencies, Feature-Unification-Semantik).
- **Optionales natives Manifest**: `Rune.toml` als Erweiterung/Alternative für Rune-spezifische Felder (z. B. CAS-Konfiguration, CI-Hooks), die Cargo ignorieren würde. `Rune.toml`-Felder überschreiben/ergänzen `Cargo.toml`, dürfen aber nie ein gültiges `Cargo.toml`-Projekt für reines `cargo build` kaputt machen.
- **Lockfile-Kompatibilität**:
  - Rune liest `Cargo.lock` und kann daraus migrieren.
  - Intern nutzt Rune ein **binäres Lockfile-Format** (Arbeitstitel `Rune.lock.bin`) für schnelleres Parsen/Diffen intern.
  - Export-Befehl (`rune lockfile export --format cargo|json|toml`) erzeugt eine lesbare, Git-diff-freundliche Repräsentation für Code-Reviews und für Projekte, die weiterhin `cargo build` als Fallback nutzen wollen.
  - **Offene Design-Frage**: Standardverhalten im Repo — wird das binäre Format oder das exportierte Textformat eingecheckt? (Siehe Abschnitt 10.)

## 6. Architektur-Überblick

```
┌─────────────────────────────────────────────────────┐
│                      rune CLI                        │
├───────────────┬───────────────┬─────────────────────┤
│  Manifest      │  Resolver     │  Command Layer      │
│  Layer         │  Engine       │  (audit, tree, why, │
│  (Cargo.toml/  │  (SAT/PubGrub │   coverage, outdated,│
│   Rune.toml    │   -basiert)   │   deny, ...)         │
│   Parser)      │               │                     │
├───────────────┴───────────────┴─────────────────────┤
│              Lockfile Layer (bin + export)            │
├───────────────────────────────────────────────────────┤
│         Fetch Layer (parallele Registry-Downloads)     │
├───────────────────────────────────────────────────────┤
│   CAS (Content-Addressable Store) — lokal & remote     │
├───────────────────────────────────────────────────────┤
│      Build Orchestrator (ruft rustc/cargo-build-plan)  │
├───────────────────────────────────────────────────────┤
│                 CI Integration Layer                   │
└─────────────────────────────────────────────────────┘
```

### 6.1 Manifest Layer
- Parser für `Cargo.toml` (muss syntaktisch/semantisch mit Cargos Verhalten übereinstimmen — Referenzimplementierung: `cargo`-Quellcode als Vergleichsbasis für Tests).
- Parser/Merger für optionales `Rune.toml`.

### 6.2 Resolver Engine
- Ziel: gleiches Ergebnis wie Cargos Resolver bei gleichem Input (Kompatibilität hat Vorrang vor Geschwindigkeit — ein schnellerer Resolver, der andere Versionen wählt als Cargo, ist ein Korrektheitsbug, kein Feature).
- Parallelisierung der Metadaten-Abfragen gegen die Registry (sparse index), aggressive lokale Caching-Schicht.
- Kandidat: PubGrub-Algorithmus (wie bei Cargos eigenem Resolver-Redesign / wie bei `uv` in Python) für nachvollziehbare Fehlermeldungen bei Konflikten.

### 6.3 Fetch Layer
- Paralleler Download von `.crate`-Dateien und Registry-Metadaten.
- Retry/Backoff-Strategie, Deduplizierung gleicher Downloads bei Multi-Package-Workspaces.

### 6.4 CAS (Content-Addressable Store)
- Speichert Build-Artefakte (kompilierte `.rlib`/`.rmeta` etc.) sowie ggf. entpackte Crate-Quellen, adressiert über Content-Hash.
- Lokaler Store standardmäßig, optionaler Remote-Store (S3-kompatibel / HTTP) für Team-/CI-Sharing — konzeptionell nah an `sccache`, aber tiefer integriert in den Build-Graph statt als reiner rustc-Wrapper.
- Cache-Key muss Compiler-Version, Flags, Feature-Set und alle transitiven Input-Hashes einschließen (Fingerprinting-Korrektheit ist kritischer als Cache-Hit-Rate).

### 6.5 Build Orchestrator
- Ruft `rustc` direkt oder über Cargos Build-Plan-Mechanismus auf (zu klären, siehe Abschnitt 10).
- Übernimmt Scheduling/Parallelisierung auf Crate-Ebene und (in Monorepo-Fällen) auf Package-Ebene mit Task-Graph, ähnlich Turborepo/moon, aber Rust-spezifisch.

### 6.6 Command Layer (eingebaute Subcommands)
Siehe Abschnitt 7 für die vollständige Liste.

### 6.7 CI Integration Layer
- Deterministische, versionierte Cache-Keys für CI-Runner.
- Eingebauter Export von Cache-Statistiken (Hit/Miss-Rate) für Debugging langsamer Pipelines.
- Optionales Remote-Execution-Modell für spätere Phasen (nicht v1).

## 7. Command-Scope

### 7.1 Cargo-Parität (muss funktionieren wie gewohnt)
`build`, `run`, `test`, `check`, `add`, `remove`, `update`, `publish`, `doc`, `clean`, `fmt`, `clippy` (delegiert)

### 7.2 Neue eingebaute Commands (bisher nur als externe Subcommands verfügbar)
- `rune audit` — Sicherheitslücken-Scan (Äquivalent zu `cargo-audit`)
- `rune outdated` — veraltete Dependencies anzeigen
- `rune deny` — Policy-Checks (Lizenzen, Bans, Duplikate) — Äquivalent zu `cargo-deny`
- `rune tree` — Dependency-Baum (Cargo hat das seit 1.44 auch eingebaut, aber UX/Filter-Optionen sollen besser werden)
- `rune why <crate>` — erklärt, warum eine Dependency im Graph ist, in npm/pnpm-Qualität
- `rune coverage` — Testabdeckung (Äquivalent zu `cargo-llvm-cov`/`cargo-tarpaulin`, eingebaut statt Add-on)
- `rune machete` — ungenutzte Dependencies finden

### 7.3 Rune-spezifische Commands
- `rune lockfile export --format <cargo|json|toml>`
- `rune lockfile migrate` — von `Cargo.lock` zu binärem Format
- `rune cas stats` / `rune cas gc` — Cache-Introspektion und Garbage Collection
- `rune ci cache-report` — CI-Cache-Diagnostik

## 8. Monorepo / Workspace-Support

- Task-Graph über Packages hinweg (welches Package hängt von welchem ab, was muss neu gebaut werden bei Änderung X).
- Selektives Ausführen: `rune test --affected` (nur betroffene Packages seit letztem Commit/Referenzpunkt).
- Pro-Package-Caching statt nur Workspace-weitem Cache.

## 9. Migration von Cargo

- `rune init --from-cargo` liest bestehendes `Cargo.toml`/`Cargo.lock`, erzeugt `Rune.lock.bin`, verändert `Cargo.toml` nicht (Rune-Projekte bleiben mit reinem `cargo build` kompatibel, sofern kein Rune-exklusives Feature genutzt wird).
- Kein Lock-in: jederzeit Export zurück zu reinem `Cargo.lock`.

## 10. Offene Design-Fragen (müssen vor Implementierung geklärt werden)

1. **Build-Ausführung**: Ruft Rune `rustc` direkt auf (volle Kontrolle, aber hoher Reimplementierungsaufwand für Build-Skripte/proc-macros/Cross-Compilation) oder nutzt es Cargos eigenen Build-Plan (`cargo build --build-plan`, experimentell/instabil) als Ausführungsschicht und optimiert nur Resolution/Fetch/Cache davor? **Empfehlung für v1: letzteres**, um Scope beherrschbar zu halten.
2. **Lockfile im Repo**: binär einchecken oder Textformat als Source of Truth mit binärem Format nur als lokaler Cache? Git-Diffbarkeit spricht stark für Text als Checked-in-Format.
3. **Resolver-Korrektheit**: Testsuite, die Rune- und Cargo-Resolver-Output auf einem großen Corpus realer `Cargo.toml`-Dateien vergleicht (Divergenzen = Bugs).
4. **CAS-Sicherheit**: Remote-Cache-Poisoning verhindern (Signierung/Verifikation von Cache-Einträgen), besonders relevant für CI-Nutzung.
5. **Scope von `Rune.toml`**: welche Felder sind exklusiv Rune (CAS-Config, CI-Hooks) vs. welche überschreiben `Cargo.toml`-Werte?

6. **Einstellungen (`rune config`) — entschieden.** Rune-spezifische Einstellungen leben in `Rune.toml` (pro Projekt, neben `Cargo.toml`, wird von Cargo ignoriert) und `~/.rune/config.toml` (global, `$RUNE_HOME` überschreibt das Verzeichnis). Rangfolge: Default < global < Projekt < `RUNE_*`-Umgebungsvariable < CLI-Flag. Alle Schlüssel sind einmal in `rune-common/src/settings.rs` (`KEYS`) deklariert; daraus folgen Validierung, `rune config list` und die typisierten Zugriffe. Erster Schlüssel mit Verhaltensänderung: `build.release-rebuild-all` (Default `false`): aus = Release-Builds nutzen den CAS wie Dev-Builds; an = alle Crates inkl. Dependencies werden für Release-Profile neu gebaut (Ergebnisse landen trotzdem im CAS). `--rebuild-all[=BOOL]` überschreibt pro Aufruf.
7. **CAS-Key-Policy — entschieden.** Registry-Crates sind unveränderlich; ihr Key kommt ohne Dateizugriff aus Name/Version/Features/Profil/Compiler und den Keys ihrer Dependencies. Lokale Pakete werden paketweit gehasht (mtime+Größe-gecacht): alle Dateien außer Build-Output-, versteckten und verschachtelten Paket-Verzeichnissen sowie Lock-Dateien und `Rune.toml`. Offen: exakte Eingaben aus rustc-Dep-Info statt Verzeichnis-Heuristik (siehe `README.md`, „Bekannte Grenzen“).

## 11. Nicht-funktionale Ziele

- **Korrektheit vor Geschwindigkeit**: jede Performance-Optimierung, die von Cargos Resolver-Verhalten abweicht, ist standardmäßig deaktiviert bzw. muss explizit opt-in sein.
- **Kein Vendor-Lock-in**: jedes Rune-Projekt muss (ggf. mit Export-Schritt) auch mit reinem Cargo weiterarbeiten können.
- **Messbare Performance-Ziele** (Platzhalter, mit Benchmarks zu belegen sobald MVP steht):
  - Resolution-Zeit auf großen Workspaces: Ziel < 50 % der Cargo-Zeit
  - Cold-Cache-Downloads: signifikant parallelisiert ggü. Cargo
  - Warm-Cache-Rebuilds über CAS: nahezu no-op bei unveränderten Inputs

## 12. Phasenplan

**Phase 1 (MVP)**: Manifest-Parser + PubGrub-Resolver mit Cargo-Paritätstests, paralleler Fetch-Layer, `rune tree`/`rune why`/`rune audit`/`rune outdated` als eingebaute Commands, Lockfile-Migration von/zu Cargo.

**Phase 2**: CAS (lokal), Build-Orchestrierung über Cargos Build-Plan, `rune coverage`/`rune deny`/`rune machete`.

**Phase 3**: Monorepo-Task-Graph, `--affected`-Selektion, Remote-CAS, CI-Integration-Layer mit Cache-Reporting.

**Phase 4 (optional, nicht garantiert)**: Toolchain-Management, Remote Execution.

## 13. Risiken

- Reimplementierung der Cargo-Resolver-Semantik ist der größte Zeit- und Fehlerrisiko-Faktor.
- Ökosystem-Adoption gegen ein offiziell mitgeliefertes Tool ist strukturell schwer — Differenzierung muss klar messbar bleiben (Performance-Benchmarks, nicht nur Feature-Liste).
- Binäres Lockfile-Format riskiert Reibung mit bestehenden Git-Workflows, falls nicht sauber gelöst (siehe 10.2).

---
*Dieses Dokument ist ein lebendiges Spec — bei größeren Architektur-Entscheidungen bitte Abschnitt 10 aktualisieren statt stillschweigend zu implementieren.*
