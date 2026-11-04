# WordCraft Collaborative

> **Proyecto cancelado (2026-11-04).** Este fork de [storytold/wordcraft](https://github.com/storytold/wordcraft)
> añadía edición colaborativa en tiempo real con un servidor propio. El trabajo queda publicado tal
> como estaba, sin fusionar en `main` (que es upstream sin cambios), para quien quiera consultarlo o
> retomarlo. El código de upstream y su licencia (MIT / Apache-2.0) no cambian.

## Qué se hizo

Varias personas editan el mismo documento de WordCraft a la vez a través de un servidor alojado por
uno mismo (homelab), con confirmación por el servidor antes de mostrar cada cambio.

| Hito | Contenido |
| --- | --- |
| C0 · Repositorio | Organización del fork, ramas, sincronización con upstream, puntos de enganche mínimos en código de upstream |
| C1 · Modelo | `crates/collab-model`: identidades de bloque, snapshots, aplicación y transformación (OT) de inserciones, borrados, divisiones y fusiones de párrafo; convergencia probada con `proptest` |
| C2 · Servidor | `apps/wordcraft-collab-server`: SQLite (WAL, migraciones), un actor por documento que persiste antes de publicar, idempotencia, recuperación, WebSocket y API HTTP (importar/exportar DOCX, historial, miembros) |
| C3 · Cliente | `crates/collab-client`: núcleo sans-I/O (compila a WASM) y transporte nativo con reconexión |
| C4 · Editor | Captura de ediciones sin ejecutar comandos, cambios remotos aplicados en la sesión, bloqueo de lo no admitido, Review › Share |
| C5 · Seguridad | OIDC (validación, PKCE en escritorio, renovación, llavero del sistema), permisos por documento en caliente, presencia y cursores remotos, límites y auditoría |
| C6 · Estabilización | Prueba de caos (cortes, duplicados, reinicios), backups cifrados con age (copia, verificación, restauración), keepalive del WebSocket, **enlaces para compartir** con invitados sin cuenta, despliegue con Caddy o Cloudflare Tunnel |
| C7 · Edición útil | Protocolo v2 con lotes atómicos y formato: **cortar, copiar y pegar** (también entre párrafos), **formato básico** (negrita, cursiva, subrayado, fuente, tamaño, color, resaltado) y **lista de documentos** del servidor en *Open Shared* |

Estado final: `cargo xtask ci` completo en verde (9 pasos, 4765 pruebas correctas, 0 fallidas) y
prueba manual con dos editores reales contra un servidor desplegado. No llegó a abrirse a usuarios
externos.

**Sin hacer:** formato de párrafo (alineación, listas, estilos), tablas e imágenes compartidas,
deshacer colaborativo, paquetes firmados del editor y la prueba con un túnel de Cloudflare real.

## Dónde está

- **Código:** rama [`collab/c7-shared-editing`](https://github.com/iisra-dev/wordcraft-collaboratory/tree/collab/c7-shared-editing)
  (todo el trabajo, C0–C7). Las ramas `collab/scaffold` (C0–C5) y `collab/c6.5-share-links` (hasta C6.5a)
  son etapas intermedias. Los PRs #1 y #14 se cerraron sin fusionar.
- **Documentación** (en esa rama): [`docs/collab/roadmap.md`](https://github.com/iisra-dev/wordcraft-collaboratory/blob/collab/c7-shared-editing/docs/collab/roadmap.md)
  (hitos, decisiones e historial), [`design.md`](https://github.com/iisra-dev/wordcraft-collaboratory/blob/collab/c7-shared-editing/docs/collab/design.md)
  (arquitectura y protocolo), [`share-links.md`](https://github.com/iisra-dev/wordcraft-collaboratory/blob/collab/c7-shared-editing/docs/collab/share-links.md),
  [`backups.md`](https://github.com/iisra-dev/wordcraft-collaboratory/blob/collab/c7-shared-editing/docs/collab/backups.md)
  y [`fork.md`](https://github.com/iisra-dev/wordcraft-collaboratory/blob/collab/c7-shared-editing/docs/collab/fork.md).
- **Editor de prueba** (macOS universal sin notarizar y Linux x86_64): release
  [`collab-v0.15.0.1-rc1`](https://github.com/iisra-dev/wordcraft-collaboratory/releases/tag/collab-v0.15.0.1-rc1).
- **Servidor:** imagen `ghcr.io/iisra-dev/wordcraft-collab-server:849fad0e` (protocolo v2, la del editor
  de prueba); `:3bc2fd2b` es la anterior (protocolo v1). Las etiquetas son los identificadores de
  commit de entonces: el historial se reescribió después solo para cambiar los correos de autor.

## Retomarlo

1. Crear una rama desde `collab/c7-shared-editing`, traer `upstream/main` (`docs/collab/fork.md` §3)
   y pasar `cargo xtask ci` con `CRAFT_I18N_DIR` y `CRAFT_FONTS_DIR` (ver `AGENTS.md`).
2. Seguir con «Dónde seguir» de `docs/collab/roadmap.md`: backup programado del despliegue,
   publicación con HTTPS y paquetes del editor (C6.3).

El proyecto principal, sus instrucciones de compilación y sus descargas oficiales están en
[storytold/wordcraft](https://github.com/storytold/wordcraft).
