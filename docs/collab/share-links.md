# Enlaces para compartir (C6.5)

> **Estado:** C6.5a, C6.5b y C6.5c implementados y validados en local; túnel real pendiente · **Última actualización:** 2026-11-04

Una invitación permite entrar en un documento como lector o editor. Solo el propietario crea,
enumera y revoca enlaces. El enlace es una **credencial de invitación**: quien lo recibe puede
obtener el acceso que concede. Compártelo únicamente con las personas previstas.

## Contrato

| Método | Ruta | Autorización |
| --- | --- | --- |
| POST | `/api/v1/documents/{id}/links` | Propietario |
| GET | `/api/v1/documents/{id}/links` | Propietario; metadatos, nunca el token |
| DELETE | `/api/v1/documents/{id}/links/{link_id}` | Propietario |
| POST | `/api/v1/links/{token}/join` | Cuenta autenticada o nombre de invitado si el enlace lo permite |
| GET | `/s/{token}` | Instrucciones de apertura, sin título ni contenido documental |

Crear acepta `role` (`editor` o `viewer`), `expires_in_seconds` (predeterminado 7 días, máximo
30 días), `allow_guests` (predeterminado `false`) y `max_guests` (1–100 si admite invitados;
predeterminado 10, y 0 si requiere cuentas). Devuelve metadatos, el token **una sola vez** y la
ruta relativa `/s/{token}`. El cliente construye la URL desde el servidor configurado, nunca
desde una cabecera Host enviada por un tercero.

Entrar acepta `{"name":"Nombre"}` para un invitado sin sesión; una cuenta usa su cabecera
`Authorization: Bearer …`. Si se envía una cabecera inválida, se rechaza: no se baja silenciosamente
a invitado. Devuelve el id del documento, usuario y rol efectivo; solo una nueva cuenta de
invitado recibe además un token de sesión. El cliente debe guardarlo en el almacén del sistema.

## Semántica y límites

- El token de invitación contiene 256 bits aleatorios. SQLite guarda exclusivamente su SHA-256;
  el UUID de gestión del enlace no sirve para entrar.
- La caducidad y la revocación impiden **entradas futuras**, no retiran pertenencias ya concedidas.
  Para retirar el acceso de alguien que ya entró, el propietario lo quita de Participants. Si
  todavía conserva un enlace válido, podrá volver a entrar: revoca también ese enlace.
- Reutilizar un enlace con la misma cuenta no degrada permisos: un propietario sigue siendo
  propietario y un editor no se convierte en lector por abrir un enlace de lectura.
- Se comprueban vigencia, cupo, alta de invitado, token de sesión, pertenencia y auditoría dentro
  de una sola transacción. Un fallo no consume plazas ni deja cuentas huérfanas. Entrar pasa por
  el actor del documento para ordenar los cambios de permiso con sus ediciones y suscripciones.
- Los invitados solo acceden al documento de su invitación: no crean documentos, no consultan
  el directorio de usuarios y no usan su sesión para aceptar invitaciones a otros documentos.
- El cupo limita las **cuentas nuevas de invitado**, no las conexiones concurrentes. Visitar
  `/s/{token}` nunca crea una cuenta ni consume una plaza.
- Se conservan como máximo 100 enlaces por documento (incluidos los revocados); se rechaza
  crear más en lugar de dejar crecer indefinidamente la lista. El cuerpo JSON tiene un límite
  de 16 KiB.
- Las respuestas con credenciales no se cachean; la página de apertura usa `Referrer-Policy:
  no-referrer`. No pongas analítica ni recursos de terceros en esa página. Configura los proxies
  para que no registren los tokens contenidos en `/s/*` y `/api/v1/links/*`.

## En el editor (C6.5b)

- **Crear:** el propietario abre **Review › Share › Participants**. En «Share link» elige *Can edit*
  o *Can view*, la duración (1, 7 o 30 días) y, si quiere, que entren personas sin cuenta con solo
  un nombre (hasta N invitados). **Copy Link** crea el enlace y lo copia al portapapeles; se muestra
  una sola vez, porque el servidor no guarda el token. La lista muestra rol, caducidad, plazas de
  invitado usadas y **Revoke**. La URL se construye con el servidor configurado en el editor.
- **Abrir:** quien lo recibe abre **Review › Share › Open Shared** y pega el enlace en «Share link».
  Con un token o una sesión iniciada entra con su cuenta; sin ella, escribe su nombre y entra como
  invitado (si el enlace lo admite). Los errores (caducado, revocado, sin plazas, requiere cuenta)
  se explican en el propio diálogo. El documento se abre en un documento nuevo del editor.
- **Sesión de invitado:** se guarda en el almacén del sistema (Keychain, Credential Manager,
  Secret Service), una entrada por servidor y documento. Si se vuelve a abrir el mismo enlace sin
  nombre ni token, se reutiliza la última sesión guardada para ese servidor; sin almacén disponible,
  el invitado entra igualmente durante la sesión. Las pruebas desactivan el almacén
  (`credentials::disable_guest_store`) para no escribir en el del equipo.
- La página `/s/{token}` indica cómo abrirlo desde WordCraft.

## Cloudflare Tunnel (C6.5c)

`deploy/compose.cloudflare.yaml` es una **alternativa**, no un overlay: reutiliza las definiciones
del servidor y de backup y añade `cloudflared`, sin arrancar Caddy ni publicar puertos en el host.

1. Copia `deploy/.env.example` a `deploy/.env`, configura `BACKUP_RECIPIENTS` y añade el
   `TUNNEL_TOKEN` de un túnel administrado en Cloudflare. Ese token es secreto: no lo subas a Git.
   Para producción, fija `CLOUDFLARED_IMAGE` a un tag/digest revisado; `latest` es solo el valor
   inicial del ejemplo.
2. En el dashboard del túnel, apunta el nombre público a `http://wordcraft-collab:3000`. Usa
   `wss://<nombre>/api/v1/ws` en el editor y conserva HTTPS para los enlaces.
3. Desde `deploy/`, valida primero y arranca solo cuando hayas preparado los directorios y las
   claves de backup como explica [`backups.md`](backups.md):
   ```bash
   docker compose --env-file .env -f compose.cloudflare.yaml config --quiet
   docker compose --env-file .env -f compose.cloudflare.yaml up -d --build
   ```

Una pantalla de inicio de sesión de **Cloudflare Access bloquea el cliente de escritorio**:
sus llamadas HTTP/WebSocket no son un navegador y no siguen ese inicio de sesión. Configura
una política **Bypass** para `/api/*` y `/s/*`, o acceso mediante el cliente **WARP**. Bypass no
desactiva la seguridad de WordCraft: su autenticación, las pertenencias y las invitaciones
siguen siendo obligatorias. Valida por separado las políticas del túnel en tu homelab; no se
han probado contra una cuenta real de Cloudflare en esta entrega.

No mezcles los dos stacks sobre los mismos datos. Para cambiar desde Caddy, para el stack
anterior antes de iniciar el alternativo (no borres volúmenes). Las órdenes de backup de la
guía siguen funcionando con `-f compose.cloudflare.yaml`.

## Pruebas de aceptación de la API

| Caso | Resultado |
| --- | --- |
| Propietario crea/enumera/revoca; lector/editor/ajeno lo intentan | Solo propietario; 403 para miembro sin permiso, 404 uniforme para ajeno/inexistente |
| Token correcto, inventado, caducado o revocado | Solo el vigente concede pertenencia; los otros dan el mismo 404 |
| Cuenta abre dos veces un enlace de lectura siendo editor/propietario | Idempotente, conserva el permiso mayor |
| Invitado sin nombre, con nombre vacío/largo/control, o cupo agotado | Rechazo sin alta, token, pertenencia ni incremento de cupo |
| Dos invitados compiten por la última plaza | Exactamente uno entra; contador y número de cuentas coherentes |
| Invitado nuevo entra y usa su token en HTTP/WebSocket | Ve su documento y respeta lector/editor; no ve documentos ajenos |
| Invitado intenta crear un documento, buscar personas o entrar en otro documento | 403 sin efectos secundarios |
| Fallo SQLite a mitad del alta | Se revierte toda la transacción, incluida la auditoría |
| Promoción por enlace con una suscripción activa | `permissions` actualizado en la FIFO del actor, sin revisión documental nueva |
| Reinicio y backup/restauración | Enlaces, caducidad, revocación, cupo y sesiones de invitado conservados |
| Inspección SQLite, lista de enlaces, auditoría, mensajes de error | Ningún token de invitación o sesión en claro |
| Página `/s/{token}` y respuestas de creación/entrada | Sin caché, sin referencias a terceros, sin contenido del documento |

## Implementación y validación

- Migración `0005_share_links.sql`: `share_links` y `guest_accounts`; no se alteran migraciones
  anteriores. El backup incluye ambas tablas sin un formato nuevo.
- `persistence/links.rs` guarda hashes, valida límites y hace el alta atómica; `api/links.rs`
  expone las cinco rutas; `DocumentHandle::accept_link` ordena el acceso con las ediciones y
  notifica los permisos después del commit.
- `tests/links.rs`: 9 pruebas correctas; incluyen dos clientes WebSocket reales (invitado lector
  e invitado editor), competencia por la última plaza, fallos SQLite y restauración cifrada.
- La configuración de Cloudflare se valida localmente sin arrancar el túnel ni usar credenciales
  reales. Falta la prueba del homelab y la interfaz del editor.
- CI final: 4744 pruebas correctas, 0 fallidas, 5 ignoradas, 49 binarios. Pasan los 9 pasos,
  incluidos assets, layers y wasm. Log local: `/tmp/opencode/wordcraft-ci-c6-links.log`.
- Se comparó `docker compose config --format json` antes/después de extraer los servicios
  comunes: servidor, backup y Caddy idénticos; la alternativa Cloudflare reutiliza exactamente
  servidor/backup y valida sin `WORDCRAFT_DOMAIN` ni puertos publicados.

## Historial

- 2026-11-04 · C6.5b · Interfaz de enlaces en Participants y *Open Shared*, sesión de invitado en el almacén del sistema y texto nuevo de `/s/{token}`. Prueba con editores reales `tests/editor.rs::share_links_from_participants_to_open_shared` (invitado que edita, cuenta con enlace de lectura, revocación) y captura headless de Participants revisada.

- 2026-11-04 · C6.5a/C6.5c · Primera entrega de servidor: API de invitaciones y alternativa
  Compose para Cloudflare. Interfaz y almacenamiento de la sesión de invitado pendientes.
