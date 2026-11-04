# Cómo se gestiona el fork

> **Última actualización:** 2026-11-03 · Fork: [`iisra-dev/wordcraft-collaboratory`](https://github.com/iisra-dev/wordcraft-collaboratory) · Upstream: [`storytold/wordcraft`](https://github.com/storytold/wordcraft)

WordCraft Collaborative es un fork de WordCraft que añade colaboración en tiempo real. Upstream avanza muy deprisa (decenas de commits al día), así que la regla de oro es **ser una capa encima de upstream, no una bifurcación**: todo lo nuestro vive en crates y documentos propios, y lo que tocamos de upstream se reduce a unos pocos puntos de enganche pequeños. Así, traer los cambios de upstream es casi siempre un merge sin conflictos.

## 1. Remotos y ramas

| Remoto | Repositorio | Para qué |
| --- | --- | --- |
| `origin` | `iisra-dev/wordcraft-collaboratory` | Nuestro trabajo; aquí se sube todo |
| `upstream` | `storytold/wordcraft` | Solo lectura: de aquí se traen los cambios |

| Rama | Contenido | Reglas |
| --- | --- | --- |
| `main` | El producto: upstream + colaboración, siempre con CI en verde | Solo se cambia por pull request; nunca `push --force` |
| `collab/<tarea>` | Una tarea (`collab/c6.1-chaos-tests`) | Sale de `main`, vuelve a `main` por PR |
| `sync/upstream-AAAA-MM-DD` | Un merge de `upstream/main` | Sale de `main`, vuelve por PR cuando el CI pasa |

Configuración local, una sola vez:

```bash
git remote add origin   https://github.com/iisra-dev/wordcraft-collaboratory.git
git remote add upstream https://github.com/storytold/wordcraft.git
git config pull.rebase false          # merges, nunca rebase de historia publicada
git config merge.conflictstyle zdiff3 # conflictos más legibles
```

## 2. Flujo de trabajo diario

```bash
git switch main && git pull                      # partir de lo último
git switch -c collab/c6.1-chaos-tests            # una rama por tarea (id de docs/collab/roadmap.md)
# … trabajo, un commit por paso con su id: "C6.1: …"
cargo xtask ci                                   # antes de subir (ver §7 para el entorno)
git push -u origin collab/c6.1-chaos-tests
gh pr create --base main --fill                  # el CI de GitHub corre en el PR
gh pr merge --merge --delete-branch              # cuando esté en verde
```

- **Merge, no squash ni rebase**, al integrar en `main`: los ids de tarea quedan en la historia, y sobre todo los merges de upstream **deben** conservar su historia o el siguiente merge de upstream se rompe.
- Cada PR cita sus tareas (`C6.1`) y actualiza [`roadmap.md`](roadmap.md) (estado y decisiones).

## 3. Traer los cambios de upstream

Hacerlo **a menudo** (idealmente cada semana; como mucho cada pocas semanas): pocos commits de upstream son un merge trivial; meses de cambios son un proyecto.

```bash
git fetch upstream
git switch main && git pull
git switch -c sync/upstream-$(date +%F)
git merge upstream/main                          # nunca rebase
# conflictos → §4; después:
cargo xtask ci
git push -u origin HEAD
gh pr create --base main --title "Sync upstream $(date +%F)" --body "Merge de upstream/main."
```

Tras integrar, comprobar especialmente: los tests de colaboración (`cargo test -p wordcraft-collab-model -p wordcraft-collab-client -p wordcraft-collab-server`), que el editor compila con los puntos de enganche (§5) y que `Cargo.lock` no duplica crates grandes.

## 4. Conflictos habituales y cómo resolverlos

| Fichero | Por qué choca | Resolución |
| --- | --- | --- |
| `Cargo.lock` | Ambos cambian dependencias | Tomar el de upstream y regenerar: `git checkout --theirs Cargo.lock && cargo update -w` (o `cargo build`) |
| `Cargo.toml` (raíz) | Upstream añade miembros o dependencias | Conservar las dos partes; nuestras dependencias están agrupadas bajo el comentario `# collaboration` |
| `crates/ui-egui/src/…` (puntos de §5) | Upstream reescribe la función donde enganchamos | Volver a colocar nuestra línea en la versión nueva de upstream; nuestro código está en `collab.rs`, `dialogs/share.rs` |
| `AGENTS.md` / `CLAUDE.md` | Upstream cambia sus instrucciones | Aceptar las de upstream y mantener nuestra sección «WordCraft Collaborative (this fork)» |
| Cualquier `docs/*.md` de upstream | No debería ocurrir: el fork no escribe en ellos (§6) | Tomar la versión de upstream |

Si un cambio de upstream rompe una API que usamos (por ejemplo `Session`, `CommandSpec::mutates`, `Document::delete_range`), se arregla en el merge, en nuestros crates, con un commit `sync: …` aparte que explique el ajuste.

## 5. Qué tocamos de upstream (puntos de enganche)

Todo lo demás es nuestro (`crates/collab-*`, `apps/wordcraft-collab-server`, `docs/collab/`, `deploy/`, `crates/ui-egui/src/collab.rs`, `crates/ui-egui/src/dialogs/share.rs`). De upstream solo se tocan estas líneas, a propósito y pocas:

| Fichero | Qué | Por qué |
| --- | --- | --- |
| `Cargo.toml` | Miembros y dependencias de colaboración | Workspace común |
| `xtask/src/layers.rs` | Capas de los crates nuevos | `cargo xtask layers` |
| `crates/ui-egui/Cargo.toml`, `lib.rs`, `app.rs` | Dependencia, `pub mod collab`, campo `collab`, `poll` | Estado de colaboración en la app |
| `crates/ui-egui/src/app/commands.rs` | `collab::command` e `collab::intercept` en `execute`; comandos sin documento | Todo comando pasa por la puerta de documentos compartidos |
| `crates/ui-egui/src/chrome.rs` | Estado y participantes en la barra de estado | Estado de sincronización siempre visible |
| `crates/ui-egui/src/canvas.rs` | `collab::paint_carets` | Cursores de otros participantes |
| `crates/ui-egui/src/dialogs*.rs`, `dialogs/{chrome,dispatch}.rs` | Variantes `Dialog::Share` y `Dialog::Participants` | Diálogos de compartir y de participantes |
| `crates/ui-egui/src/ribbon/review.rs`, `icons.rs` | Grupo Review › Share e iconos | Botones |
| `crates/ui-egui/src/widgets/{buttons,tooltip}.rs`, `control/handler.rs`, `window_ui.rs` | Botones desactivados con motivo; comandos descubribles; estado común de la app | Coherencia de la interfaz |
| `crates/layout/src/tests/pages.rs` | Test de paginación independiente de las fuentes del sistema | Arreglo candidato a upstream (§8) |
| `AGENTS.md`, `CLAUDE.md`, `.gitignore` | Sección del fork; secretos de `deploy/` | Instrucciones y seguridad |

**Reglas para no ampliar esta lista:** el código nuevo va en nuestros crates o módulos; en código de upstream, una llamada de una línea a nuestro módulo, nunca lógica; no reformatear ni «mejorar» código de upstream de paso; si hace falta cambiar algo de upstream de verdad (un bug, una API), proponerlo a upstream (§8).

## 6. Documentación

- Lo nuestro, en español: [`design.md`](design.md) (diseño), [`roadmap.md`](roadmap.md) (tareas, estado, decisiones), este fichero.
- **No escribir en la documentación de upstream** (`ROADMAP.md`, `docs/gaps.md`, `docs/*-parity.md`…): sus líneas de estado cambian en cada commit de upstream y cualquier añadido nuestro choca en cada sincronización. Excepción tolerada: `docs/control-protocol.md` documenta los comandos `ui.collab.*` al final, donde upstream rara vez escribe.
- La portada del fork en GitHub es [`.github/README.md`](../../.github/README.md) (GitHub la muestra en lugar del `README.md` de upstream, que no se toca).

## 7. CI

En el fork corren los workflows de upstream que no necesitan secretos de storytold:

| Workflow | En el fork | Motivo |
| --- | --- | --- |
| `ci.yml` (gate: fmt, clippy, tests, wasm…) | ✅ activo | La puerta de calidad de `main` y de cada PR |
| `printing.yml`, `packaging-lint.yml`, `nix.yml` | ✅ activos | Sin secretos; solo corren si cambian sus rutas |
| `release.yml` | ⛔ desactivado | Necesita los certificados de firma de storytold |
| `sonarcloud.yml` | ⛔ desactivado | Necesita un proyecto SonarCloud propio |
| `freebsd.yml`, `windows-arm64.yml` | ⛔ desactivados | Trabajos nocturnos largos; se reactivan cuando haya releases propias |
| `collab-desktop.yml` (**propio del fork**) | ✅ activo | Acordado con el propietario (2026-11-04): al subir una etiqueta `collab-v*` construye el DMG universal de macOS (firma ad-hoc, sin notarizar) y el `.tar.gz` de Linux x86_64 (Ubuntu 22.04, glibc 2.35) y los publica como *prerelease* para probadores. Archivo nuevo: no choca con upstream |

Se desactivan en GitHub (`gh workflow disable …`), **no editando los ficheros**, para que no choquen al sincronizar. Los workflows no se modifican en el fork salvo que se acuerde expresamente.

Localmente, `cargo xtask ci` necesita los datos de idioma y las fuentes (las pruebas de corrección y de paginación los usan):

```bash
git clone https://github.com/storytold/craft-i18n.git ../craft-i18n   # y checkout de craft-i18n.rev
git clone --depth 1 https://github.com/storytold/craft-fonts.git ../craft-fonts
export CRAFT_I18N_DIR=$PWD/../craft-i18n CRAFT_FONTS_DIR=$PWD/../craft-fonts
cargo xtask ci
```

## 8. Lo que conviene enviar a upstream

Arreglos que no son de colaboración se proponen a upstream (PR a `storytold/wordcraft` desde una rama basada en `upstream/main`), para no cargarlos para siempre:

- [ ] `crates/layout/src/tests/pages.rs`: la regresión de interlineado múltiple depende de la sustitución de fuentes del sistema; usar el fixture de fuentes incluidas.
- [ ] Etiquetas `mutates` de `edit.repeat`, `insert.editHeader` e `insert.editFooter`: están marcados como puros pero cambian el documento (encontrado por `collab-client/tests/gate.rs`).
- [ ] `cmd::packs::tests::swiss_german_and_portugal_portuguese_keep_their_languages_rules` falla sin `CRAFT_I18N_DIR` en lugar de saltarse como los demás tests de idioma.

## 9. Versiones y releases

- La versión del workspace sigue la de upstream (`0.15.0`…). Las releases del fork se etiquetan **`collab-v<versión upstream>.<n>`** (p. ej. `collab-v0.15.0.1`) desde `main`, con notas que digan qué upstream incluyen y qué cambia de colaboración.
- El servidor se distribuye como imagen Docker (`apps/wordcraft-collab-server/Dockerfile`, `deploy/`); el cliente, cuando haya paquetes (C6.3).

## 10. Seguimiento

- El trabajo pendiente está en [`roadmap.md`](roadmap.md) y en los issues del fork, agrupados por milestone (C6, mejoras). Un issue por tarea; el PR lo cierra (`Closes #n`).
- Los ids de tarea (`C6.1`) van en ramas, commits, PR e issues, para encontrar todo lo de una tarea.
