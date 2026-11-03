# WordCraft Collaborative

Colaboración en tiempo real para [WordCraft](https://github.com/storytold/wordcraft), el procesador de textos de código abierto escrito en Rust: varias personas editan el mismo documento desde sus ordenadores, con un **servidor propio** (homelab) y sin servicios en la nube.

> Fork de [`storytold/wordcraft`](https://github.com/storytold/wordcraft). Todo lo de WordCraft sigue igual; el README original está en [`README.md`](../README.md).

## Qué hace

- **Edición compartida** de texto y párrafos (escribir, borrar, Intro, Retroceso) con convergencia probada entre réplicas; los cambios concurrentes incompatibles se rechazan sin perder datos.
- **Servidor autoritativo** (Axum + SQLite): persiste cada cambio antes de confirmarlo, snapshots con checksum, recuperación tras reinicios, importación y exportación DOCX.
- **Permisos por documento** (propietario, editor, lector), **inicio de sesión** con OIDC (Authentik) o tokens locales, **presencia** (quién está y dónde escribe), límites y auditoría.
- **En el editor:** Review › Share para compartir o abrir un documento, estado de sincronización siempre visible y cursores de los demás.

Fuera del MVP por ahora: formato, tablas, deshacer y selecciones entre párrafos en documentos compartidos.

## Empezar

```bash
# servidor (datos en ./data)
cargo run -p wordcraft-collab-server
cargo run -p wordcraft-collab-server -- create-user Ana      # token de acceso, se muestra una vez
# editor
cargo run --release -p wordcraft                              # Review › Share › Share Document
```

Despliegue con Docker Compose y Caddy: [`deploy/`](../deploy) · Configuración: [`deploy/.env.example`](../deploy/.env.example).

## Documentación

- [Diseño técnico](../docs/collab/design.md) — arquitectura, protocolo, modelo de operaciones, seguridad.
- [Hoja de ruta y decisiones](../docs/collab/roadmap.md) — hitos C1–C6, estado y por qué de cada decisión.
- [Cómo se gestiona el fork](../docs/collab/fork.md) — ramas, sincronización con upstream, CI, releases.

## Licencia

La misma que WordCraft: MIT o Apache-2.0 ([`LICENSE-MIT`](../LICENSE-MIT), [`LICENSE-APACHE`](../LICENSE-APACHE)).
