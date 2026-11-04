# Enlaces para compartir (C6.5)

> **Estado:** contrato de la API; implementación pendiente · **Última actualización:** 2026-11-04

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
- Las respuestas con credenciales no se cachean; la página de apertura usa `Referrer-Policy:
  no-referrer`. No pongas analítica ni recursos de terceros en esa página. Configura los proxies
  para que no registren los tokens contenidos en `/s/*` y `/api/v1/links/*`.

La interfaz de copiar/abrir enlaces, el almacenamiento de la sesión de invitado y el despliegue
Cloudflare se entregan después de validar esta API. No se presenta C6.5 como completo antes.

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
