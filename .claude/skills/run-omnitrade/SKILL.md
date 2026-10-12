---
name: run-omnitrade
description: Run, start, build, drive and screenshot the omnitrade desktop app (omnitrade-ui, egui) and its CLI (omnitrade-cli validate/backtest). Use when asked to run, start, launch, screenshot, click through, test the GUI, press the NOT-AUS / kill switch, or smoke-test a strategy backtest.
---

# run-omnitrade

omnitrade ist ein Rust-Workspace: `omnitrade-ui` (egui/eframe-Desktop-GUI, reine Anzeige plus Not-Aus-Flag-Datei) und `omnitrade-cli` (validate, backtest, live, kill). Die GUI wird unter einem virtuellen X-Server (Xvfb) gestartet und per `xdotool` bedient; Screenshots entstehen mit ImageMagick `import`.

**Pfade in dieser Datei sind relativ zum Repo-Root** `/home/user/omnitrade` (dort liegt `Cargo.toml`).

## Agent-Pfad (zuerst lesen)

Alles läuft über den Driver `.claude/skills/run-omnitrade/driver.sh`:

```bash
.claude/skills/run-omnitrade/driver.sh build      # cargo build --workspace
.claude/skills/run-omnitrade/driver.sh flow       # Xvfb + GUI + Screenshot + NOT-AUS setzen/zuruecksetzen
.claude/skills/run-omnitrade/driver.sh cli        # OKX-Kerzen holen, validate, Backtest
.claude/skills/run-omnitrade/driver.sh stop       # GUI und Xvfb beenden
```

Einzelbefehle: `start`, `shot [name]`, `killswitch`, `reset`, `status`, `fetch`.

- Screenshots: `target/run-omnitrade/*.png` (`01_start.png`, `after_killswitch.png`, `after_reset.png`). Nach jedem Schritt mit `Read` ansehen, nicht nur den Exit-Code prüfen.
- Ein eigener Befehl: `driver.sh shot mein_name` erzeugt `target/run-omnitrade/mein_name.png`.
- Eigene Klicks: `xdotool mousemove --window "$(xdotool search --name omnitrade | head -1)" X Y click 1` mit `DISPLAY=:99`. Koordinaten relativ zum Fenster (Fenster bei 0,0, 1280x800).
- Die GUI läuft mit `OMNITRADE_KILL_FILE=target/run-omnitrade/omnitrade.kill`, damit das Flag-File nicht in `/tmp` landet.

Erwartete Ergebnisse von `flow`:
- `01_start.png`: OKX-BTC-USDT-Kerzen (199 historische Kerzen), Kopfzeile `getrennt`, `NOT-AUS` rot.
- Nach `killswitch`: Kopfzeile `NOT-AUS GESETZT` plus Button `Zurücksetzen`, Datei `omnitrade.kill` existiert.
- Nach `reset`: Datei weg, `NOT-AUS` wieder frei.

## Prerequisites

Einmalig, als root, im Linux-Container der Session. Diese Zeile habe ich ausgeführt, Exit 0:

```bash
apt-get update -qq && apt-get install -y -qq libxkbcommon0 libxkbcommon-x11-0 libxcb-xkb1 libxcb-render0 libxcb-shape0 libxcb-xfixes0 libxcb-cursor0 libxi6 libxcursor1 libxrandr2 libxinerama1 libgl1 libegl1 libwayland-client0 x11-apps imagemagick xdotool
```

Xvfb, `curl`, `python3`, Rust (cargo 1.97.0 getestet) müssen vorhanden sein.

## Build

```bash
cargo build --workspace
```

Erwartung: `Finished dev profile`. Erststart kann mehrere Minuten dauern (Abhängigkeiten).

## Run: CLI (headless, kein Display nötig)

```bash
./target/debug/omnitrade-cli validate --script .claude/skills/run-omnitrade/sample_buy.omni
./target/debug/omnitrade-cli backtest --script .claude/skills/run-omnitrade/sample_buy.omni --data target/run-omnitrade/btc_1h.csv --initial-balance 1000000
```

- Erwartung `validate`: `Script validation successful`, Exit 0. Fehlerhafte Syntax: `ParseError`, Exit 1.
- Erwartung `backtest`: Tabelle `=== Backtest Results ===`, Exit 0.
- `driver.sh cli` holt die Kerzen (`fetch`) und führt beides aus.

## Run: GUI (Agent-Pfad über Driver)

```bash
.claude/skills/run-omnitrade/driver.sh flow
```

Der Driver startet Xvfb auf `:99`, dann `./target/debug/omnitrade-ui BTC-USDT testnet --exchange okx`, wartet auf das Fenster und klickt NOT-AUS bzw. Zurücksetzen.

## Run: GUI (Mensch-Pfad)

Auf einem Rechner mit echtem Display dasselbe Binary `omnitrade-ui` mit den Argumenten `BTC-USDT testnet --exchange okx` starten. Im Container nur über Xvfb getestet, nicht auf Windows/macOS.

## Gotchas

- **Binance ist aus diesem Container gesperrt** (`testnet.binance.vision` und `api.binance.com` liefern HTTP 451). Deshalb `--exchange okx` verwenden.
- **OKX-WebSocket ist nicht erreichbar.** `wss://ws.okx.com:8443` wird vom Proxy nach ca. 7 s mit Code 1006 geschlossen (Proxy-Log: `ws_closed_mid_exchange`). REST auf Port 443 funktioniert. Folge: Kopfzeile `getrennt`, `Orderbook empty`, keine Live-Ticker. Die GUI ist in diesem Container nur mit historischen REST-Kerzen prüfbar. Ob das an der Umgebungs-Netzwerkpolicy oder am Proxy liegt, ist nicht abschließend geklärt.
- **Die GUI braucht `libxkbcommon-x11`.** Ohne sie bricht sie in `winit` mit `Library libxkbcommon-x11.so could not be loaded` ab, bevor ein Fenster erscheint.
- **Keine `pkill -f` / `pgrep -f` mit dem Skript- oder Befehlsnamen.** Das Muster trifft die eigene Shell, Exit 144. Stattdessen `pgrep -x omnitrade-ui` bzw. `pgrep -x Xvfb`.
- **Hintergrundprozesse brauchen `</dev/null` und eigene Redirects.** Sonst hält die GUI die stdout-Pipe des Aufrufers offen, und der Befehl kehrt nie zurück. Der Driver macht das bereits so.
- **Backtest mit Indikatoren ist auf HEAD defekt.** `strategie.omni` mit `rsi(14)` liefert `EvalError: insufficient candles (1) for rsi with period 14`, obwohl 100 Kerzen geladen sind. Der Warmup-Fix liegt in `origin/main` (Commit `e63ef02`, `fix(backtest): indicator warmup skip`) und ist nicht in HEAD. Ein Cherry-pick kollidiert mit HEAD in `omnitrade-cli/src/backtest.rs` und `omnitrade-engine/src/account.rs`. Im Driver steht deshalb nur ein Smoke-Test ohne Indikator.
- **Backtest-Standardguthaben 10 000 reicht nicht.** Die Standardgröße (1 BTC bei ca. 81 000 USD) liefert `InsufficientBalance { required: ..., available: 10000.0 }`. `--initial-balance 1000000` setzen.
- **Der Kill-Switch-Pfad ist `$TMPDIR`-abhängig.** Ohne `OMNITRADE_KILL_FILE` schreibt die GUI nach `temp_dir()/omnitrade.kill`. Der Driver setzt die Variable.
- **Klickpunkte sind layoutabhängig.** `NOT-AUS` bei (381,11), `Zurücksetzen` bei (504,11). Nach einer UI-Änderung neu per Screenshot prüfen und die Konstanten in `driver.sh` anpassen.

## Troubleshooting

| Symptom | Ursache | Fix |
|---|---|---|
| `Library libxkbcommon-x11.so could not be loaded` | fehlendes Paket | Prerequisites-Zeile ausführen |
| `unable to open X server` (import) | Xvfb läuft nicht | `driver.sh start` oder `driver.sh flow` |
| `kein GUI-Fenster nach 40 s` | GUI abgestürzt | `target/run-omnitrade/ui.log` lesen |
| `insufficient candles (1) for rsi` | Warmup-Bug in HEAD | Strategie ohne Indikator testen oder `main` mergen (siehe Gotchas) |
| `InsufficientBalance` im Backtest | Standardguthaben zu klein | `--initial-balance 1000000` |
| Exit 144 bei `pkill`/`pgrep` | Muster trifft die eigene Shell | `pgrep -x <name>` verwenden |
| `Connection ended` / `ws_closed_mid_exchange` | OKX-WS-Port 8443 gesperrt | erwartet, nicht behebbar im Container |
| `Konto-Anzeige gibt es nur für Binance` | Kontodaten nur für Binance | erwartet bei `--exchange okx` |

## Stand

- Getestet: Build, `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace` (grün), GUI-Start und Screenshot, NOT-AUS setzen/zurücksetzen per Klick, CLI validate und backtest.
- Nicht getestet: Live-Handel (Testnet-Schlüssel fehlen, Binance gesperrt), Kraken/Binance-Marktdaten, Windows/macOS.
