# Fonts

The app uses **Geist** (400/500/600) and **Geist Mono** (400/500), OFL-licensed, from
<https://github.com/vercel/geist-font/releases>. Put the static `.ttf` files and `OFL.txt` here; `make app-run` loads
them from this folder and `scripts/package-app.sh` copies them into `Cordata.app/Contents/Resources/fonts`.

Without them the window uses the system fonts (SF Pro, then SF Mono or Menlo) — `app/crates/cordata-app/src/theme.rs`
(`load_fonts`) picks whichever is present. Nothing is downloaded at run time.
