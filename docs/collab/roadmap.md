# WordCraft Collaborative: hoja de ruta y estado

> **Última actualización:** 2026-11-04 · **Rama:** `collab/c7-shared-editing` (apilada sobre `collab/c6.5-share-links`, pendiente de integración por PR) · **Siguiente tarea:** prueba manual en el homelab de C7 y C6.5b (imagen nueva del servidor y cliente: protocolo v2), luego PR a `main` · Diseño: [`design.md`](design.md) · Gestión del fork: [`fork.md`](fork.md)

Los ids de tarea (`C1.2`…) van en los mensajes de commit, como pide `AGENTS.md` (`C1.2: apply InsertText/DeleteText`). Antes de cada commit: `cargo xtask ci`.

## Estado

| Hito | Estado |
| --- | --- |
| C0 · Preparación del repositorio | ✅ hecho |
| C1 · Modelo colaborativo | ✅ hecho |
| C2 · Servidor autoritativo | ✅ hecho (falta `POST …/snapshots`, versiones con nombre) |
| C3 · Cliente nativo básico | ✅ hecho |
| C4 · Integración con el motor | ✅ hecho (comandos, diálogo de conexión y cinta) |
| C5 · Seguridad y experiencia | ✅ hecho |
| C6 · Estabilización | 🟡 C6.1, C6.2, C6.4 y C6.5 (API, UI y configuración Cloudflare) validados en local; homelab/túnel real y C6.3 pendientes |
| C7 · Edición compartida útil | ✅ C7.1–C7.5 validados en local (CI completa en verde); falta la prueba en el homelab y el PR |

## C0 · Preparación del repositorio (hecho)

- Crates `collab-protocol` (L0 standalone), `collab-model` (L2), `collab-client` (L5) y la app `wordcraft-collab-server`, registrados en `xtask/src/layers.rs`.
- `collab-protocol`: ids, `DocumentOperation`, mensajes `ClientMessage`/`ServerMessage` con envelope versionado, límite de tamaño y versión comprobados antes de parsear, `RejectReason`.
- `collab-model`: `BlockIdentityMap` (cuerpo, nivel superior), conversión `Pos` ↔ `Anchor`, `transform` con la regla segura (bloques disjuntos pasan, el resto es `Conflict`), `apply` que de momento rechaza todo con `Unsupported`.
- `collab-client`: `CollaborationState`, `PendingQueue` acotada e idempotente.
- Servidor: config por entorno, SQLite WAL con migraciones por `user_version`, `/api/v1/health/{live,ready}`, subcomando `healthcheck`, test de integración.
- `deploy/` (Compose, Caddyfile, `.env.example`), `Dockerfile`, `.dockerignore`.

### Decisiones tomadas al preparar el repo

| Decisión | Motivo |
| --- | --- |
| Los ids (`BlockId`…) viven en `collab-protocol`, no en `collab-model` | Las operaciones del protocolo los necesitan y el protocolo no puede depender del modelo documental |
| `collab-protocol` es *standalone* (sin crates del workspace) | Lo comparten cliente, servidor y tests; compila para wasm |
| `collab-client` es sans-I/O por ahora y aún no depende de `wordcraft-engine` | Está en L5, así que `cargo xtask wasm` lo comprueba: el transporte tokio irá tras la feature `transport` (C3.1); la dependencia del motor llega con C4.1 |
| `uuid` sin `v4`/`v7` en el workspace | Los ids los generan los binarios; los crates compartidos no necesitan fuente aleatoria (inyectada en `BlockIdentityMap::assign`) |
| `rusqlite` (bundled) en lugar de `sqlx` | Síncrono y simple para un actor por documento; sin macros que necesiten la BD al compilar |
| Migraciones con `PRAGMA user_version` | Sin herramienta externa; una migración por fichero, nunca se editan tras publicarse |
| `tracing` en el servidor | Ecosistema Axum/Tokio; el resto del repo sigue con `log` |
| Remoto `origin` renombrado a `upstream` | El fork propio será `origin`; los cambios de upstream se integran con merge desde `upstream/main` |

### Decisiones de C1

| Decisión | Motivo |
| --- | --- |
| `BlockIdentityMap` no implementa `Deserialize`; solo se reconstruye con `restore` | Un registro leído de fuera siempre se valida contra su documento |
| El snapshot lee primero `format` y luego el resto con `deny_unknown_fields` | Un snapshot de otra versión se informa como tal, no como JSON mal formado |
| `InsertText` no admite U+FFFC ni caracteres de control salvo `\t` y `\n`; texto vacío es error | `insert_text` descarta U+FFFC en silencio (el tamaño cambiaría) y los saltos de párrafo viajan como `SplitParagraph` |
| `DeleteText` no puede cubrir objetos incrustados (`Unsupported`) | `delete_range` conserva marcadores de controles de contenido: se borrarían menos bytes que `end - start` y las transformaciones de offsets fallarían |
| El texto insertado toma el formato a su izquierda (`props_at`) | Igual que al escribir en el editor; es determinista en todas las réplicas |
| Empate insert/insert en el mismo offset: la confirmada antes queda primero | Solo el servidor transforma en el MVP, así que el orden del servidor es el mismo en todas las réplicas; un cliente optimista futuro usará la misma regla al rebasar sus pendientes |
| Una inserción dentro de un rango borrado concurrentemente se coloca donde colapsó el borrado | Se conserva el texto del usuario; es la regla habitual de OT |
| Un borrado que contiene una inserción confirmada estrictamente dentro es `Conflict` | Ni se borran palabras ajenas ni se deja medio rango sin borrar; el usuario repite sobre el texto actual |
| `MergeParagraphs` lleva `first_len` (longitud de `first` antes de combinar), validado por `apply` | `transform` solo ve operaciones: sin la longitud no puede rebasar ediciones en `second`; además hace la operación invertible para el undo colaborativo |
| La fusión usa `delete_range` desde el final de `first` hasta el inicio de `second`, y se comprueba el resultado | Es la misma primitiva que Retroceso al inicio de párrafo (saltos de sección incluidos); si alguna vez uniera de otra forma, se rechaza y se restaura |
| Split y merge se aplican con copia de seguridad (`atomically`) | Varias primitivas + registro: todo o nada; los bloques van en `Arc`, la copia no duplica párrafos |
| Dos fusiones idénticas concurrentes: la segunda se convierte en un borrado vacío | El autor ve su operación confirmada y su cola avanza; el resultado ya es el que quería |
| Un borrado que atraviesa un punto de división concurrente es `Conflict`; un split, en cambio, se mueve al punto de colapso de un borrado | Una operación no puede borrar en dos párrafos; dividir donde colapsó el borrado conserva la intención |
| Una división de `f` frente a una fusión `f`+`g` confirmada es `Conflict` | `f` y `g` ya no son adyacentes |
| TP1 se garantiza para el texto, no para las propiedades de párrafo en «fusión + división de `g`» | El párrafo nuevo hereda las propiedades de quien se divide; todas las réplicas siguen el orden del servidor y acaban idénticas |
| `document_operations.client_id` (migración `0002`, no se edita la `0001`) | Un reenvío idempotente debe devolver el `committed` original, que incluye el `client_id` |
| `commit_operation` recibe la revisión que espera el actor y falla si el documento está en otra | Las revisiones son densas (1, 2, 3…) y un segundo escritor sería un fallo; se detecta en lugar de escribir mal |
| Un `operation_id` repetido devuelve el original aunque el reenvío traiga otra base u otro contenido | Lo que cuenta es el id: la operación ya se confirmó y no se vuelve a aplicar |
| La propiedad no se transfiere con `set_member`; el propietario no se puede quitar | Un documento tiene exactamente un propietario; transferirlo será una acción propia |
| Cada actor corre en un hilo bloqueante (`spawn_blocking` + `blocking_recv`) | SQLite es síncrono; un hilo por documento abierto basta para un homelab y no bloquea el runtime |
| Al arrancar, el actor reproduce el log de SQLite sobre el estado base y comprueba que llega a `current_revision` | Es la recuperación tras un reinicio; un log con huecos o que no encaja con la base impide arrancar en vez de servir un estado dudoso |
| El duplicado (`operation_id` ya registrado) se comprueba tras el permiso y antes que la revisión | Un reenvío tras reconectar puede traer una base antigua; debe recibir su confirmación original, no `StaleRevision` |
| Historial en memoria acotado (`MAX_HISTORY` = 10 000); más atrás, `submit` da `StaleRevision` y `resync` un snapshot | Memoria acotada por documento |
| Cola por suscriptor de 256 mensajes; si se llena, se le descarta (su canal se cierra) | Backpressure: un cliente lento no frena a los demás; al reconectar hace `resync` |
| Nuevo `RejectReason::Unavailable` | Fallo de almacenamiento: no cambió nada y el cliente puede reintentar la misma operación |
| Los lectores pueden unirse y recibir commits; solo propietario y editores envían | Diseño §12/§13 |
| Snapshots cada 100 commits y al cerrar el actor; sin temporizador de 5 min | El actor espera en `blocking_recv` y no tiene reloj; el log ya guarda cada commit, así que el snapshot solo acorta la recuperación. El temporizador puede llegar con C2.3 |
| Un snapshot ilegible (clave rara, checksum, formato) o que no encaja con el log se salta y se prueba uno anterior | La recuperación nunca sirve un estado dudoso; el de revisión 0 existe siempre desde la creación |
| `create` rechaza un id ya existente antes de escribir en disco | Encontrado en los tests: reescribir el fichero de revisión 0 de un documento existente dañaba su estado |
| Autenticación local con tokens opacos (`wct_` + 32 bytes aleatorios) mientras no hay OIDC | Permite probar con varios usuarios reales en el homelab; en la base de datos solo está el SHA-256 y el token viaja en cabecera, nunca en la URL |
| Un documento inexistente y uno sin permiso dan la misma respuesta (`forbidden`) | No se pueden sondear ids de documentos |
| Un commit nuevo llega al autor por su suscripción; un duplicado se le responde directamente | Un solo camino para los commits publicados; el duplicado no se publica y solo lo necesita quien lo reenvió |
| La presencia vive solo en el actor: se difunde, nunca se persiste ni provoca guardado | §10; una sesión revocada o cerrada desaparece de la lista |
| Cada participante tiene un color fijo derivado de su `client_id` | El mismo cursor se reconoce siempre por su color; los colores son oscuros para que el nombre en blanco se lea |
| `tokio-tungstenite` 0.29 + `futures-util` en el workspace, de momento solo como dependencias de desarrollo del servidor | Tests extremo a extremo; el transporte nativo del cliente (C3.1) usará el mismo crate. La versión coincide con la que ya trae axum |
| El historial HTTP devuelve el tipo de operación, no su texto | Auditoría sin contenido innecesario (§12); el contenido ya lo ven los miembros en el documento |
| Los ids de documentos y bloques creados por el servidor son UUID v4 de `getrandom` | Sin `uuid/v4` en el workspace (los crates compartidos no generan ids); si faltara la fuente aleatoria, la creación falla limpiamente |
| Reconectar es `join` (snapshot nuevo) + reenvío de pendientes, no `resync` | El servidor exige `join` en cada conexión antes de aceptar `submit`; el snapshot ya cubre lo que se perdió y los duplicados se responden con el commit original. `resync` queda para huecos dentro de una conexión |
| Un commit que no encaja en la réplica la descarta y pide snapshot | El servidor lo aplicó, así que la réplica divergió: nunca se edita sobre una suposición |
| C5.2: `snapshot.can_edit` determina el permiso inicial; `permissions` lo actualiza en caliente | Sin capacidad explícita, el cliente nuevo queda en solo lectura. Un snapshot autorizado posterior puede restaurar la edición; `forbidden` sigue cerrándola |
| Límites de tamaño distintos por sentido: 1 MiB lo que recibe el servidor, 64 MiB lo que recibe el cliente | Un `snapshot` lleva el documento entero, imágenes incluidas; con un único límite, los documentos con imágenes no se podrían abrir |
| TLS del cliente con rustls y el almacén de certificados del sistema | Funciona con una CA propia del homelab o con certificados de Tailscale sin configuración extra |
| La prueba extremo a extremo del transporte vive en el crate del servidor | El servidor (exento de capas) puede depender del cliente; al revés sería una dependencia hacia arriba |
| La captura deriva las operaciones de la selección y la réplica; no ejecuta el comando ni compara documentos | Con confirmar-antes-de-aplicar el documento local no debe cambiar hasta el commit; el motor queda intacto (merges fáciles con upstream) |
| Una edición es una operación + como mucho una continuación, colocada con el commit de la primera | Una segunda operación calculada de antemano partiría de «réplica + primera», un estado que el servidor no tuvo si transformó la primera; la continuación se ancla al borrado tal como se confirmó, y es exacta aunque haya ediciones concurrentes |
| Sin selecciones entre párrafos en documentos compartidos (por ahora) | Necesitan varias continuaciones encadenadas; se rechazan explícitamente |
| Sin Autocorrección ni Autoformato al escribir, e Intro/Retroceso no cambian listas, estilos ni formato de párrafo en documentos compartidos | El MVP solo comparte texto y estructura de párrafos; esas reglas cambian formato, que aún no tiene semántica de concurrencia |
| Escribir rápido antes de que llegue el commit usa la réplica sin confirmar: dos Retrocesos seguidos pueden borrar un solo carácter | Latencia aceptada por el diseño (§6) hasta la vista especulativa local; las inserciones en el mismo punto sí conservan el orden por el desempate del servidor |
| La maquetación se invalida con `Session::touch` (API pública existente) | La clave de la caché es la revisión de la sesión; asignar `Session::doc` sin más dejaría la maquetación desfasada. `touch` también marca el documento como modificado, lo que de momento se acepta |
| Una inserción ajena justo en el caret lo deja delante; una propia, detrás; un Intro propio lleva el caret al párrafo nuevo | Es lo que espera quien escribe; el resto del texto ajeno no «empuja» al caret |
| El filtro se basa en `CommandSpec::mutates`, pero no se fía a ciegas: `run_pure` compara el documento y lo restaura | Tres comandos de upstream marcados como puros sí lo cambian; se bloquean por nombre y el barrido falla si aparece otro. Sus etiquetas no se corrigen aquí para no alejar el fork de upstream |
| `ui.collab.*` son comandos programáticos sin diálogo (de momento) | Regla del repositorio: las llamadas programáticas nunca abren diálogos; el diálogo de conexión será la capa de interfaz encima |
| Un documento compartido a la vez; sus eventos esperan mientras se muestra otro documento | Los eventos solo se pueden aplicar a la `Session` del documento compartido; si la cola se llena, el servidor descarta al cliente y este se pone al día al volver |
| El cliente nativo corre en su propio hilo con un runtime Tokio de un hilo (`ThreadedClient`) | La interfaz egui es síncrona: solo `try_send`/`try_recv`, nunca bloquea un fotograma |
| Publicar usa `ureq` (ya en el árbol por `craft-update`) sin la feature `json` | Evita añadir crates (la feature `json` metía `cookie_store`) |
| El actor no retiene la conexión SQLite mientras transforma y aplica | La conexión es común a todos los documentos; los cambios de miembros ya van por el actor, así que el rol no cambia entre la comprobación y el commit |
| Perder la lectura vacía también las operaciones pendientes del cliente | No se reenvía nada a un documento al que ya no se tiene acceso |
| Validación de JWT con `jsonwebtoken` (backend `aws-lc-rs`, ya en el árbol por rustls), no hecha a mano | Biblioteca muy usada que protege de la confusión de algoritmos; solo se aceptan RS*/PS*/ES* |
| El primer acceso con un `sub` nuevo crea el usuario | En el homelab la identidad la gestiona Authentik; el acceso a cada documento sigue dependiendo de la pertenencia |
| Un fallo del proveedor (caído, respuesta rara) se trata como token no válido (401) y se registra sin el token | Nunca se acepta un token sin verificar; el token no aparece en los registros |
| Inicio de sesión de escritorio: cliente público con PKCE S256 y loopback en `127.0.0.1` con puerto libre (RFC 8252) | Sin secreto de cliente en la app; el código no sirve sin el verificador |
| El campo del token del diálogo admite 8192 caracteres | Un JWT de Authentik pasa de mil; con 200 se truncaba al pegarlo |
| Solo se guarda el token de refresco (con emisor y client id), nunca el de acceso | El de acceso dura minutos y se renueva; si no hay almacén del SO, la sesión funciona en memoria y no se escribe nada en otro sitio |
| La sesión guardada se busca al abrir el diálogo de compartir, no al arrancar | El almacén puede pedir desbloquearse: no se molesta a quien no usa la colaboración; siempre fuera del hilo de la interfaz |
| Tras un 401 se renueva una sola vez por rechazo | Un inicio de sesión revocado no hace girar al proveedor |
| `keyring` 4.2 (Secret Service sobre `zbus` 5, la versión que ya usa la app) | Sin duplicar la pila D-Bus; Keychain y Credential Manager nativos en macOS y Windows |
| La auditoría registra acciones e ids, nunca contenido; las ediciones ya están en `document_operations` | §12: auditoría sin datos sensibles innecesarios; ni siquiera el título |
| Los límites se aplican antes de leer el mensaje y no cierran una conexión que se modera | Un pegado grande o una reconexión con pendientes caben en la ráfaga; un cliente desbocado o malicioso no tumba el servidor |
| Las ediciones remotas no generan revisiones de control de cambios | Pendiente de C4: el motor decide si marcar la edición local antes de convertirla en operación |
| Al publicar se pausa la edición hasta cargar el snapshot inicial | El DOCX ya está enviado: aceptar cambios locales durante el POST haría que el snapshot los sobrescribiera |
| Conectar no reemplaza una conexión o publicación existente | Se exige desconectar explícitamente para no abandonar operaciones pendientes ni cambiar la ventana activa por error |
| Servidor y token se recuerdan solo en memoria; el token no aparece en serialización ni `Debug` del formulario | El gestor de credenciales del SO sigue pendiente de C5.1; las URL no aceptan credenciales, query ni fragmento |
| Cerrar el receptor de eventos cancela también el handshake y las escrituras de red | Desconectar o cerrar la aplicación no puede bloquear el hilo de la UI esperando un servidor que no responde |
| Los cambios de miembros pasan por el actor del documento | Se ordenan con las ediciones y las suscripciones; la autorización y el commit comparten el bloqueo de SQLite |
| Revocar un miembro envía `permissions` sin lectura y elimina sus suscripciones | No recibe commits posteriores. No se puede retirar una copia que ya recibió o descargó legítimamente; los mensajes previamente autorizados pueden estar en tránsito |
| `resync` WebSocket: con la suscripción viva devuelve el permiso actual y solo los commits perdidos; sin ella (descartada por backpressure, revocada o nunca unida) se une de nuevo con un snapshot | Un cliente atrasado no recibe el documento entero (hasta 64 MiB con imágenes) para unos pocos commits; el permiso que pudo perderse con la cola llena llega delante. Respuestas, snapshots y eventos siguen en la misma FIFO |

## C1 · Modelo colaborativo (hecho)

Objetivo (diseño §16): dos documentos independientes que reciben las mismas operaciones terminan idénticos, sin red.

- ✅ **C1.1** Registro de identidades completo: asignación inicial desde un `Document`, serialización en el snapshot colaborativo (documento + ids), restauración validada. `SharedDocument` (`collab-model/src/snapshot.rs`): `{format, document, blocks}` en JSON, formato versionado (`SNAPSHOT_FORMAT`), campos desconocidos rechazados, registro comprobado contra el cuerpo (número de bloques, ids únicos con un conjunto, sin coste cuadrático) y runs normalizados como al abrir JSON nativo.
- ✅ **C1.2** `apply`: `InsertText` y `DeleteText` sobre `wordcraft_doc` (`insert_text`, `delete_range`), validando rango, límites de carácter y objetos incrustados; sin cambios si hay error. Propiedad `proptest`: dos réplicas con las mismas operaciones quedan idénticas y un error no cambia nada.
- ✅ **C1.3** `transform` insert/insert en el mismo bloque con desempate determinista (diseño §5.3), luego insert/delete y delete/delete. Cada regla con tests unitarios y `proptest`. Tabla de reglas en `collab-model/src/transform.rs`; propiedad TP1 (ambos órdenes del servidor convergen salvo el empate, que respeta el orden del servidor) y ninguna inserción se pierde.
- ✅ **C1.4** `SplitParagraph` y `MergeParagraphs` en `apply` (actualizando el registro) y sus transformaciones, o `Conflict` documentado. `MergeParagraphs` lleva `first_len`; las operaciones estructurales son atómicas (copia de documento y registro); propiedad TP1 con dos párrafos, divisiones y fusiones.
- ✅ **C1.5** Prueba de convergencia: N réplicas + "servidor" en memoria con secuencias aleatorias (`proptest`), incluido Unicode multibyte. Propiedad: `D_A = D_B = D_servidor`. `collab-model/tests/convergence.rs`: 3 clientes que editan sobre réplicas desfasadas y un cuarto que entra tarde desde un snapshot; documento y registro idénticos al servidor; tras transformar solo se admite `Conflict` como rechazo. Ejecución larga fija: 2469 commits y 48 conflictos en 3000 pasos.
- ✅ **C1.6** Lo mismo con dos `Session` reales de `wordcraft-engine` (prueba de concepto del §16). `collab-client/tests/sessions.rs` (el motor es solo dependencia de desarrollo): dos sesiones entran desde un snapshot, reciben los commits en `Session::doc` sin `set_document`, siguen maquetando y quedan idénticas al servidor; después, los mismos comandos reales dan el mismo documento. Con el documento de muestra (22 bloques, una tabla): 313 commits en 400 pasos.

## C2 · Servidor autoritativo (hecho)

- ✅ **C2.1** Repositorio SQLite: usuarios, documentos, miembros, operaciones (inserción transaccional, unicidad por `operation_id`). `persistence/repository.rs`: `commit_operation` escribe la revisión siguiente y mueve `current_revision` en una transacción, exige la revisión esperada por el actor y devuelve el commit original ante un `operation_id` repetido; un único propietario por documento; textos de usuario validados; filas corruptas dan error, nunca pánico. Migración `0002` (`client_id` por operación, índice de miembros por usuario).
- ✅ **C2.2** `DocumentActor`: canal Tokio, copia de trabajo, persistir antes de publicar, idempotencia. `documents/actor.rs` + `tests/actor.rs`: inserciones concurrentes que convergen y llegan a todos (autor y lectores incluidos), reenvío aplicado una vez, rechazos sin cambios (lector, ajeno, revisión futura, offset inválido, conflicto), reinicio que conserva revisión y contenido, participante lento descartado y `resync`, fallo de almacenamiento sin cambios y log incoherente rechazado al arrancar.
- ✅ **C2.3** Endpoint `/api/v1/ws`: `hello`/`join`/`submit`/`resync`/`presence`, límites de tamaño, backpressure. `ws/mod.rs`: autenticación antes del upgrade (401), `hello` primero, `client_id` del `submit` igual al del `hello`, `join` antes de `submit`, un registro (`documents::Registry`) con un solo actor por documento, cola de salida acotada por conexión y aviso `stale_revision` si el actor descarta al participante. Autenticación local provisional hasta OIDC (C5.1): `create-user` imprime un token una vez y solo se guarda su SHA-256 (migración `0003`). `tests/websocket.rs`: tres clientes reales que convergen, ACK perdido recuperado sin duplicar, autenticación y validación de mensajes.
- ✅ **C2.4** Snapshots atómicos con SHA-256; recuperación = último snapshot válido + operaciones posteriores; test de reinicio. Hecho antes que C2.3, porque el endpoint necesita abrir documentos. `storage/mod.rs` (temporal → fsync → rename → fsync del directorio; claves validadas; lectura acotada), `documents::open` (el snapshot más reciente que se lee íntegro y encaja con el log; si no, uno anterior), `documents::create` (snapshot de revisión 0 + filas en una transacción). El actor guarda cada 100 commits y al pararse. `tests/recovery.rs`.
- ✅ **C2.5** API HTTP de documentos (crear/importar DOCX, listar, metadatos, exportar, historial, miembros). `api/documents.rs` + `tests/http.rs`: `POST /api/v1/documents` en JSON (en blanco) o DOCX (`?title=`, máx. 64 MiB), lista por usuario, metadatos, exportación DOCX del estado compartido, historial sin texto (revisión, id, autor, cliente, tipo) y miembros solo por el propietario. Inexistente y ajeno dan el mismo 404. Pendiente: `POST …/snapshots` (versiones con nombre).

## C3 · Cliente nativo básico (hecho)

- ✅ **C3.1** Transporte WebSocket nativo tras la feature `transport` (tokio-tungstenite + rustls), fuera del build wasm. `connection.rs`: una tarea Tokio dueña del `SharedSession`, token en cabecera, rustls con los certificados del sistema, mensajes del servidor hasta `MAX_SERVER_MESSAGE_BYTES` (64 MiB, para snapshots); la interfaz usa `Client` (`submit`, `acknowledge_conflict`, `replica`, `close`, eventos).
- ✅ **C3.2** Coordinador: `join` → snapshot → `CollaborationState::Synchronized`; aplicar `committed`. Hecho antes que C3.1: `SharedSession` (`coordinator.rs`) es sans-IO — recibe `ServerMessage` y devuelve mensajes a enviar y eventos para el editor — y ya incluye la lógica de reconexión de C3.3 (`connected` = `hello` + `join` + reenvío de pendientes). Commits en orden estricto (antiguos ignorados pero liquidan su pendiente; hueco → `resync`), conflicto que pausa la edición hasta reconocerlo, lector en solo lectura, réplica divergente → `join` nuevo.
- ✅ **C3.3** Reconexión: `resync` desde la última revisión confirmada y reenvío de `PendingQueue`. Espera exponencial (reiniciada al conectar); al reconectar, `join` + reenvío de pendientes (ver decisiones). `apps/wordcraft-collab-server/tests/client.rs`: dos clientes reales convergen, el servidor cae y vuelve en el mismo puerto, ambos se reconectan solos y siguen editando.

## C4 · Integración con el motor

- ✅ **C4.1** Captura `CommittedEdit` en el límite de `Session::run` para los comandos admitidos (escritura, Retroceso/Supr, Intro). `collab-client/src/capture.rs`: `text.insert`, `text.tab`, `text.lineBreak`, `text.backspace`, `text.delete`, `text.newParagraph` se convierten en operaciones a partir de la selección y la réplica, sin ejecutar el comando y sin tocar el motor; cada edición se valida sobre una copia. Una operación por edición; reemplazar una selección de un párrafo envía el borrado y una continuación (`FollowUp`) que se coloca donde el servidor colapsó el borrado. `tests/capture.rs`.
- ✅ **C4.2** `apply_remote_edit` sin `set_document`: identidades, selección/anclajes, invalidación de layout, sin evento saliente. `collab-client/src/bridge.rs` (`SessionBridge`, con su propio registro al paso de `Session::doc`) y `anchors::transform_anchor` en `collab-model`: la inserción ajena en el caret lo deja delante y la propia lo mueve detrás; un caret en una tabla conserva su celda; `Session::touch` invalida la maquetación; nada entra en el historial de deshacer. `tests/bridge.rs`.
- ✅ **C4.3** Bloquear comandos no admitidos y undo/redo en documentos compartidos (con motivo visible en la UI). `collab-client/src/gate.rs`: con `CommandSpec::mutates` del propio registro, los admitidos se capturan, deshacer/rehacer y los demás que modifican se bloquean con un motivo para la UI, y los puros se ejecutan con `run_pure`, que restaura el documento si alguno lo cambiara. Un barrido de todo el registro (`tests/gate.rs`) encontró tres mal etiquetados (`edit.repeat`, `insert.editHeader`, `insert.editFooter`), que se bloquean por nombre. Mostrar los motivos en la UI llega con C4.4.
- ✅ **C4.4** Comandos y UI: conectar, abrir/publicar documento compartido, estado de sincronización siempre visible. `crates/ui-egui/src/collab.rs` con `ui.collab.connect` (abre en un documento nuevo), `ui.collab.publish` (POST del DOCX en un hilo y conexión), `ui.collab.disconnect` (queda copia local), `ui.collab.acknowledge`, `ui.collab.copyId` y `ui.collab.status`; descubribles en `engine.commands`. `intercept` delante del motor en `WordApp::execute` (captura, bloqueo con motivo en la línea de estado, comandos puros vigilados); `poll` por fotograma con `SessionBridge`; estado en la barra de estado (clic tras un conflicto), incluida la publicación. Review › Share ofrece publicar, abrir, copiar id y desconectar, con iconos originales; `ui.dialog` abre `sharePublish` / `shareOpen` (servidor, token oculto y título/id, validación sin cambiar el documento). Las URL inválidas se rechazan antes de abrir otra ventana; el formulario no serializa el token. `apps/wordcraft-collab-server/tests/editor.rs`: dos `WordApp` reales publican, abren, escriben y convergen contra un servidor real, con título Unicode y protección de la publicación/conexión activa. Tests de formulario y cancelación en `dialogs/share.rs`; transporte: validación de credenciales, codificación UTF-8 de títulos y desconexión durante handshake bloqueado. Revisión visual mediante `ui_shot`, sin controlar el escritorio.

## C5 · Seguridad y experiencia

- ✅ **C5.1** OIDC Authorization Code + PKCE con Authentik (navegador del sistema, callback loopback), validación de tokens en el servidor, token en el gestor de credenciales del SO. Hecho (C5.1a, servidor): `auth/oidc.rs` valida los tokens de acceso (descubrimiento con `issuer` comprobado, JWKS en caché con refresco limitado a uno por minuto, solo algoritmos asimétricos, `iss`/`aud`/`exp`/`nbf`/`sub` con 60 s de margen, tamaños y tiempos acotados) y da de alta al usuario en su primer acceso por `sub`; los tokens locales siguen funcionando. `tests/oidc.rs` contra un proveedor simulado. Hecho (C5.1b, cliente): `collab-client/src/oidc.rs` (feature `transport`) con Authorization Code + PKCE S256 y redirección loopback `http://127.0.0.1:<puerto>/callback` (en Authentik, permitirla con cualquier puerto), `state` comprobado, tiempo límite, canje y refresco; `ui.collab.signIn` y la sección «Sign in with an identity provider» del diálogo de compartir rellenan el token (en memoria). `apps/wordcraft-collab-server/tests/pkce.rs` contra un proveedor simulado que verifica el reto. Hecho (C5.1c): `oidc::TokenSource` renueva el token de acceso con el de refresco un minuto antes de caducar o tras un 401 (el transporte pide el token antes de cada intento); `credentials.rs` guarda emisor, client id y token de refresco en el almacén del SO (Keychain, Credential Manager, Secret Service) con `keyring` 4, y la sesión se restaura al abrir el diálogo de compartir; `ui.collab.signOut` la olvida. Tests: renovación contra el proveedor simulado y reconexión tras 401.
- ✅ **C5.2** Permisos por documento en todas las operaciones y endpoints implementados; viewers en solo lectura desde la primera conexión. `snapshot.can_edit` (aditivo, `false` si falta en el protocolo v1) y `permissions {can_read, can_edit}` en caliente; promociones, degradaciones y revocación sin generar revisiones documentales. `DocumentHandle::change_member` ordena la API de miembros con las ediciones, protege al propietario y cierra las suscripciones revocadas; `read(user)` autoriza la exportación dentro del actor y el historial vuelve a autorizar bajo el mismo bloqueo de su consulta. WebSocket comprueba `join`, `submit`, `resync` y `presence`; cada `resync` renueva la suscripción con snapshot y capacidad actuales, en una FIFO única. El cliente no envía ediciones de lectores ni continuaciones de reemplazos tras una degradación; una revocación invalida su réplica para nuevos commits. La UI comparte el motivo de bloqueo entre ejecución, botones, tooltips y `engine.commands`. Regresiones: capacidad ausente/solo lectura inicial, cambios en caliente, revocación, matriz de rutas HTTP y WebSocket, y `WordApp` real de lector que pasa a editor tras reconectar. OIDC, presencia visual y límites por usuario siguen fuera de este objetivo.
- ✅ **C5.3** Presencia: participantes y cursores remotos con frecuencia limitada. Servidor (C5.3a): el actor guarda nombre y cursor por cliente (nunca se persiste), los difunde, los manda a quien entra, los mueve con cada commit, descarta cursores que no encajan, limita a una actualización cada 50 ms por cliente y a 64 participantes, y quita a quien se desconecta o pierde el acceso. Cliente (C5.3b): el coordinador guarda los demás participantes y mueve sus cursores con los commits; la interfaz envía su cursor al cambiar (máx. 5/s), muestra «N more here» con los nombres al pasar el ratón y dibuja cada cursor ajeno con su color y su nombre. Revisado con captura contra un servidor real.
- ✅ **C5.5** Pantalla de participantes, para invitar sin `curl`. Servidor: `GET /api/v1/documents/{id}/members` (cualquier miembro), `GET /api/v1/users?q=` (búsqueda por nombre o id; comodines literales; máx. 20) y `GET /api/v1/me`. Cliente: `collab-client/src/people.rs`, peticiones HTTP bloqueantes con errores explicados y sin token. Editor: comandos `ui.collab.members`, `findPeople`, `invite` y `removeMember`, que se ejecutan en segundo plano y de uno en uno; diálogo **Review › Share › Participants** con lista de acceso, rol editable, Remove, búsqueda e Invite (solo el propietario puede cambiar el acceso; los demás ven la lista). Tests: HTTP (`people_to_invite_and_who_has_access`) y dos `WordApp` reales contra un servidor (`the_owner_invites_from_the_participants_screen`). Capturas `ui_shot` revisadas en claro y oscuro.
- ✅ **C5.4** Mensajes de error y conflicto comprensibles; límites por conexión/usuario; auditoría. C5.4a: cada motivo de rechazo tiene una frase para la persona (qué pasó y qué hacer), el texto técnico va al registro; nuevo `RejectReason::RateLimited` (HTTP 429). C5.4b: cubo de fichas por conexión (ráfaga 200, 50/s; >500 rechazos cierra), máx. 16 documentos por conexión y 16 conexiones por usuario. C5.4c: tabla `audit_log` (migración `0004`) con altas de usuario, creación, cambios de miembros y exportaciones, sin contenido; `GET /api/v1/documents/{id}/audit` solo para el propietario.

## C6 · Estabilización

- **C6.1** ✅ Tests de integración con tres clientes, desconexiones aleatorias, duplicados y reinicios (`apps/wordcraft-collab-server/tests/chaos.rs`; véase design.md §13). Correcciones que salieron al hacerlo: la revisión base de una edición es la que muestra el editor, no la del hilo de transporte (`SharedSession::submit_at`, `submit_edit(base_revision, …)`); un `operation_id` rechazado sigue rechazado si se reenvía (el actor recuerda sus rechazos definitivos); y tras un reinicio el actor recupera su historial desde el registro, también lo anterior al snapshot, para que las ediciones en vuelo no se rechacen como `StaleRevision`.
- **C6.2** ✅ Backups cifrados y restauración probada localmente, en `collab/c6.2-backups` (homelab real pendiente):
  - `src/backup.rs` y los subcomandos `backup [--every 24h]`, `verify-backup <file|latest> --identity <key|->` y `restore <file> --identity <key|-> --to <dir vacío>`;
  - servicio `backup` en `deploy/compose.yaml` y guía en [`backups.md`](backups.md);
  - 4 tests de integración en `tests/backup.rs` y 3 regresiones unitarias de seguridad en `src/backup.rs`, que pasan; otra regresión de CLI rechaza intervalos Unicode sin pánico;
  - revisión 2026-11-04: la API de backup de SQLite copia solo a memoria (antes quedaba una base de datos temporal sin cifrar en la carpeta sincronizada); temporales privados, publicación sin sobrescribir, límite agregado de extracción y manifiesto acotado.

  La imagen anterior se construyó (154 MB) y se probó con Compose local: servicio `backup` cada minuto, retención de 3 copias, `verify-backup` y `restore` con la clave por la entrada estándar (`--identity -`). Tras la revisión de seguridad pasan CLI, tests y los 9 pasos de `cargo xtask ci`; falta reconstruir esa imagen y probar la restauración en el homelab real. Al construir la imagen se necesitó `--network=host` para resolver DNS en esta máquina.
- **C6.3** Empaquetado del cliente para Linux, Windows y macOS.
- **C6.4** ✅ Pings WebSocket para que Cloudflare Tunnel y otros proxies no corten las conexiones inactivas (cortan a los ~100 s). El cliente envía un ping cada `TransportConfig::keepalive` (25 s por defecto) y da la conexión por perdida tras tres intervalos sin oír nada (`connection.rs`, `drive`). `run_connection` atiende los comandos también mientras conecta; `Client::close` cancela la tarea de transporte, incluso con handshake, escritura o cola de eventos bloqueados y aunque el receptor de eventos se haya movido a otra tarea. Intervalos públicos acotados para evitar desbordamientos del reloj y de la espera exponencial; el límite de silencio usa el mismo intervalo mínimo que los pings. Pasan los 7 tests de `connection` (pings, reconexión, cierre, consumidor separado, backpressure, configuración y codificación). La CI encontró un fallo de Clippy y un bloqueo al cerrar los consumidores separados del test de caos; ambos corregidos, con 3 rondas completas de caos correctas (720 ediciones) y la CI completa en verde.
- **C6.5** 🟡 Compartir como en Word. Objetivo: dar a amigos una URL detrás de Cloudflare One y que entren «con un enlace y ya». Contrato, seguridad y despliegue: [`share-links.md`](share-links.md).
  - **C6.5a · Servidor, implementado:** migración `0005` con `share_links` (solo SHA-256 del token, documento, rol, caducidad, cupo y revocación) y `guest_accounts` (sesión limitada a un documento). `POST/GET /api/v1/documents/{id}/links` y `DELETE …/links/{link_id}` solo para el propietario; `POST /api/v1/links/{token}/join` para cuentas o invitados permitidos. Alta, token de sesión, pertenencia, cupo y auditoría en una transacción dentro del actor; promoción con `permissions` y sin revisión documental. No degrada el permiso existente. El invitado no crea documentos, no consulta el directorio y no acepta invitaciones a otros documentos; tampoco puede añadirse a otro documento mediante Participants. Revocar/caducar el enlace cierra nuevas entradas, no borra pertenencias ya concedidas. `GET /s/{token}` sirve instrucciones sin contenido documental y advierte que la UI todavía no acepta enlaces. Respuestas privadas, JSON de 16 KiB, hasta 100 enlaces/documento y 100 invitados/enlace. Pasan 9 tests en `tests/links.rs` (HTTP, WebSocket real lector/editor, competencia por cupo, rollback, revocación en vuelo, backup/restauración), Clippy y los 9 pasos de la CI completa (4744 pruebas correctas).
  - **C6.5b · Cliente/UI, implementado:** en Participants (propietario), «Share link» con *Can edit*/*Can view*, 1/7/30 días e invitados sin cuenta (cupo); **Copy Link** lo copia y lo muestra una vez; lista con caducidad, plazas y **Revoke**. *Open Shared* acepta el enlace: con cuenta/token entra como esa cuenta; sin ella pide un nombre de invitado. Sesión de invitado en el almacén del SO (por servidor y documento) y reutilizada al reabrir el enlace. `/s/{token}` explica cómo abrirlo. Prueba de editores reales `share_links_from_participants_to_open_shared` y CI completa en verde (4765 correctas, 0 fallidas). Detalles: [`share-links.md`](share-links.md).
  - **C6.5c · Despliegue, configuración implementada:** alternativa `deploy/compose.cloudflare.yaml`, con `cloudflared` (`TUNNEL_TOKEN`) en lugar de Caddy. Servidor y backups comparten `compose.services.yaml`; se comprobó que las definiciones resueltas del stack Caddy no cambian y que Cloudflare no publica puertos ni necesita `WORDCRAFT_DOMAIN`. Guía de Bypass `/api/*` y `/s/*` o WARP en `share-links.md`. No se ha arrancado un túnel ni probado una cuenta Cloudflare real.

## C7 · Copiar/pegar, formato básico y documentos del servidor (validado en local)

**Prioridad acordada con el propietario:** este es el próximo bloque de desarrollo, antes de
la interfaz de enlaces C6.5b. No implica que las opciones ya estén habilitadas en el cliente
instalado ni en la imagen `3bc2fd2b` del servidor.

**Alcance de esta sesión:** solo plantear y documentar el trabajo; no implementar ni ejecutar
pruebas. Cuando se autorice el desarrollo, completar primero C7.1–C7.4 y dejar las pruebas
para C7.5, antes de instalar o publicar una versión nueva. El diseño exacto de las operaciones
y la estrategia de compatibilidad se decidirán en C7.1; no se da por aprobado un protocolo nuevo.

**Borrador de código apartado por petición del propietario:** stash local `f445bf02682d4a55c582e6738a61253105cf78b6`
(`stash@{0}` al crearlo), mensaje `C7: inicio parcial de protocolo y formato; incompleto, sin
compilar ni probar; solo planificar`. Contiene únicamente `collab-protocol/src/{lib,operations}.rs`
y el archivo nuevo `collab-model/src/format.rs`, bajo `crates/`. Es un inicio incompleto y no
integrado de operaciones atómicas/formato; no acredita C7.1 como hecho ni fija el protocolo.
No está aplicado en el árbol de trabajo ni publicado. La hoja de ruta y `data/` quedan fuera
del stash. Para recuperarlo sin eliminarlo, cuando se autorice continuar:
`git stash apply f445bf02682d4a55c582e6738a61253105cf78b6` (revisar antes el estado del árbol).

- **C7.1 · Base de sincronización:** definir las operaciones documentales y sus transformaciones
  para rangos y propiedades de carácter; validar anclajes, UTF-8, tamaños y permisos. Negociar
  capacidades o versionar el protocolo para que un cliente/servidor antiguo nunca ignore un
  cambio que no entiende. No transmitir comandos dependientes de la selección local.
- **C7.2 · Copiar y pegar (primera función visible):** comprobar/habilitar Ctrl+C y Ctrl+V y el
  menú contextual sobre el portapapeles del sistema, tanto entre documentos de WordCraft como
  con otras aplicaciones. Primero texto sin formato: Unicode, varios párrafos, inserción y
  reemplazo de selección, también entre párrafos. El lector puede copiar, pero no pegar.
  Un pegado rechazado no puede borrar la selección ni dejar una modificación parcial; limitar
  el tamaño del contenido y conservar el orden/intención ante ediciones concurrentes.
- **C7.3 · Formato básico y pegado con formato admitido:** negrita, cursiva, subrayado, fuente,
  tamaño, color de texto y resaltado. Capturar sus efectos, aplicarlos/persistirlos en el
  servidor y reflejarlos en todas las réplicas. Conservar esas propiedades al copiar/pegar
  entre documentos compatibles; el contenido externo no admitido debe rechazarse con una
  explicación u ofrecerse explícitamente como texto, nunca interpretarse como operaciones
  arbitrarias del editor.
- **C7.4 · Documentos del servidor:** pantalla autenticada para listar, buscar por título y
  abrir los documentos accesibles sin copiar sus IDs. Carga en segundo plano, errores y lista
  vacía explicados; solo aparecen documentos a los que el usuario tiene acceso. Mantener la
  apertura por ID como alternativa avanzada.
- **C7.5 · Validación y entrega (al final del bloque):** implementar primero C7.1–C7.4;
  después añadir/actualizar y ejecutar todas las pruebas. Pruebas de modelo/convergencia y
  Unicode; permisos de lector;
  dos editores reales con formato y pegados concurrentes; límites y errores sin cambios
  parciales; desconexión/reenvío sin duplicar el pegado; reinicio, backup/restauración y
  exportación/reapertura DOCX conservando texto y formato. Revisar atajos/menús y capturas
  headless y listado/apertura sin IDs; `cargo xtask ci` completo. Preparar versiones nuevas de
  servidor y cliente para probar en el homelab, sin reemplazar la etiqueta inmutable `3bc2fd2b`.

**Validación C7.5 (2026-11-04):** `cargo xtask ci` completo en verde: 9 pasos, 4763 pruebas
correctas, 0 fallidas, 5 ignoradas. Nuevas pruebas: `collab-model` (lotes, formato, proptest de
convergencia de lotes), `collab-client/tests/c7.rs` (captura de pegado/corte/formato/selección
entre párrafos), gate, `tests/editor.rs::two_editors_paste_format_cut_and_list` (dos editores
reales con negrita y pegado concurrentes, corte entre párrafos y repegado con formato, listado
por permisos, lector sin pegar), exportación DOCX con formato (`tests/http.rs`) y la prueba de
caos con formato y lotes (5 rondas con cortes, duplicados y reinicios). Captura headless de
*Open Shared* revisada (sin servidor con datos). **No probado:** túnel/homelab real, portapapeles
de otras aplicaciones en un escritorio real y pegado desde HTML/RTF externo con un servidor.

**Estado de la implementación:**
- **C7.1** `82d6e4e2`: operaciones `format_text` (parche de formato básico validado en
  `collab-model/src/format.rs`: nombres de subrayado/resaltado de WordCraft, color `RRGGBB`/`auto`,
  tamaño en medios puntos, fuente acotada) y `batch` (varias operaciones en una revisión, todo o
  nada; planas, máx. 4096 pasos). Transformación paso a paso; un empate al arrastrar la operación
  concurrente es `Conflict` (se reintenta sobre el texto actual). **Protocolo v2**: un cliente o
  servidor v1 es rechazado en `hello` en lugar de ignorar cambios. El borrador del stash
  `f445bf02` no se usó (quedó sustituido); el stash sigue sin borrar.
- **C7.2/C7.3** `7cb1e37a`: Pegar, Solo texto y Combinar formato resuelven el portapapeles como el
  editor y se convierten en un único lote (borrar selección, dividir párrafos, insertar tramos y
  darles formato). Tablas, objetos y pegados de más de 256 KiB se rechazan con explicación.
  Cortar copia en local y comparte el borrado. Las selecciones entre párrafos se borran/reemplazan
  como lote. Negrita, cursiva, subrayado (y doble/palabras), fuente, tamaño, color y resaltado se
  capturan como `format_text` sobre la selección (sin selección se explica que hay que seleccionar).
  Pendiente de revisar: el texto pegado con formato toma las propiedades resueltas (estilos de
  carácter incluidos) pero no el estilo; aumentar/reducir fuente sigue bloqueado.
- **C7.4** `9af0ff79`: *Open Shared* muestra «Tus documentos» (`GET /api/v1/documents`, en segundo
  plano), con rol y fecha, búsqueda por título y apertura con un clic; lista vacía y errores
  explicados. La apertura por ID queda en «Abrir por ID de documento».

**Fuera de esta primera entrega:** pegar tablas o imágenes como objetos, formato de párrafo
(alineación, espaciado, sangrías, listas y estilos), estructuras avanzadas, comentarios y
deshacer/rehacer colaborativo. Los controles sin semántica implementada y probada siguen
bloqueados; no se habilitan simplemente quitando el filtro de comandos.

**Criterio de cierre:** dos usuarios pueden copiar/pegar texto de varios párrafos y aplicar el
formato básico admitido y abrir documentos desde el listado sin divergencias ni pérdida de
cambios confirmados; el lector sigue sin poder editar. Después se retoma C6.5b y el resto de
estabilización pendiente.

## Dónde seguir (2026-11-04)

1. ✅ Test de C6.4 arreglado y `Close` atendido durante `connect()` (2026-11-03, último paso hecho).
2. ✅ CI completa de C6.2/C6.4: 4735 pruebas correctas, 0 fallidas y 5 ignoradas en 48 binarios; 9 pasos correctos, incluido WASM. Usar `CRAFT_I18N_DIR=$(realpath ../craft-i18n) CRAFT_FONTS_DIR=$(realpath ../craft-fonts) cargo xtask ci` (`CRAFT_FONTS_DIR` apunta a la raíz, no a `fonts/`). Log local: `/tmp/opencode/wordcraft-ci-c6-close.log`.
3. ✅ Commits C6.2 `58edff32`, C6.4 `e0f4859d` y C6.5a/C6.5c `3bc2fd2b`, publicados en `origin/collab/c6.5-share-links`; no integrados en `main`. Imagen GHCR `ghcr.io/iisra-dev/wordcraft-collab-server:3bc2fd2b` publicada para linux/amd64 (la descarga anónima requiere visibilidad pública del paquete).
4. ✅ CI de C6.5a/C6.5c en verde (`/tmp/opencode/wordcraft-ci-c6-links.log`): 4744 pruebas correctas, 0 fallidas, 5 ignoradas en 49 binarios; 9 pasos correctos. C6.5b queda pendiente para después de C7: el servidor expone la API, pero la UI actual aún no acepta pegar esos enlaces. No declarar C6.5 completo sin esa entrega.
4b. ✅ C7.1–C7.5 en `collab/c7-shared-editing` (commits locales, sin publicar); CI completa en verde (4763 correctas, 0 fallidas, 5 ignoradas). **← Siguiente paso:** el propietario decide publicar la rama/imagen (protocolo v2: servidor y clientes deben actualizarse juntos) y probar en el homelab; después C6.5b.
5. No tocar `data/` en la raíz del repo: es la base de datos de una prueba manual del propietario. Está ignorada por Docker y no se commitea.

## Cómo trabajar

```bash
# tests de los crates de colaboración
cargo test -p wordcraft-collab-protocol -p wordcraft-collab-model -p wordcraft-collab-client -p wordcraft-collab-server
# servidor local (datos en ./data)
cargo run -p wordcraft-collab-server
curl localhost:3000/api/v1/health/ready
# usuario local con token (se muestra una sola vez); el cliente lo envía en `Authorization: Bearer …`
cargo run -p wordcraft-collab-server -- create-user Ana
# antes de commitear
cargo xtask ci
```

Ramas, sincronización con upstream, conflictos habituales, CI del fork y releases: [`fork.md`](fork.md). Los cambios en crates existentes (`engine`, `ui-egui`) se limitan a los puntos de enganche listados allí, para que los merges de upstream sigan siendo fáciles.

### Probar desde la interfaz de escritorio

1. Arrancar el servidor y crear un usuario local con los comandos anteriores (el token se muestra una sola vez).
2. En **Review › Share › Share Document**, introducir `ws://127.0.0.1:3000/api/v1/ws` para una prueba local, el token y un título. Para un servidor remoto, preferir `wss://…/api/v1/ws` detrás de TLS.
3. Esperar a **Shared · up to date**. Durante el POST y la conexión la edición está pausada; una publicación fallida conserva el documento local.
4. **Participants** (Review › Share) muestra quién tiene acceso. El propietario busca a la persona por su nombre (al menos dos letras), elige *Can edit* o *Can view* y pulsa **Invite**. También puede cambiar el rol o quitar el acceso desde la lista. Una persona que entra con OIDC aparece después de su primer inicio de sesión. Los usuarios locales aparecen en cuanto se crean.
5. **Copy Document ID** copia solo el UUID. La persona invitada entra desde **Open Shared**, con su propio token y ese UUID. Copiar el id no da acceso: el acceso se da en Participants.
6. **Stop Sharing** conserva el documento como copia local y vuelve a habilitar la edición normal. Si se cancela un POST ya enviado, el documento puede haberse creado en el servidor; no se promete deshacer esa creación.

### Dónde reside el documento y cómo subir un archivo existente

El **servidor del homelab es quien aloja y conserva la copia compartida**. No hay un participante
que tenga que mantener WordCraft abierto para «hostearla».

1. Una persona abre su archivo local en WordCraft y usa **Review › Share › Share Document**.
   El editor envía una representación DOCX del documento activo a `POST /api/v1/documents`;
   el servidor lo importa, persiste su estado inicial y devuelve un id nuevo.
2. El propietario invita a las demás personas en **Participants**. Cada una abre **Open Shared**
   con el mismo servidor, su propio token y ese id. Estar dado de alta en el servidor no da
   acceso automáticamente a todos sus documentos.
3. Las ediciones confirmadas se guardan en el servidor antes de notificarse a los clientes.
   Quien lo publicó puede cerrar su editor o apagar su ordenador: los demás siguen editando
   mientras el servidor y sus conexiones estén disponibles.

En el contenedor, la persistencia normal está en `/data/wordcraft.db` y `/data/documents`,
respaldadas por el volumen/directorio montado del homelab. El formato autoritativo es el
modelo interno, sus identidades y el registro de operaciones; **no** un DOCX que alguien
deba copiar manualmente dentro de esas carpetas. DOCX es importación/exportación.

El archivo original del ordenador no se convierte en una carpeta sincronizada: editarlo
después en otra aplicación no actualiza por sí solo la copia compartida. Publicarlo otra vez
crea otro documento/id; para continuar el mismo trabajo se usa **Open Shared**, no una segunda
publicación. El servidor permite exportar el estado compartido a DOCX. Todavía falta en el
cliente un explorador/listado de documentos del servidor (incluido en C7.4) y una subida directa
sin abrir antes el archivo; la API sí tiene listado, creación/importación y exportación.

**Pendiente C7.4:** «Todavía falta una pantalla para listar y abrir los documentos del servidor sin copiar sus IDs.»

El token local no se guarda en preferencias y debe volver a introducirse tras reiniciar; OIDC sí puede restaurar la sesión desde el almacén del sistema (C5.1). Los comandos `ui.collab.connect` / `publish` siguen siendo programáticos: no abren diálogos. La interfaz web no tiene transporte colaborativo. Solo se comparten las ediciones de texto y párrafos admitidas; no formato ni undo/redo. El lector ve **Shared · read-only** desde el primer snapshot; promocionarlo/degradarlo actualiza las capacidades en caliente. Tras revocar y volver a conceder lectura debe abrir o resincronizar el documento de nuevo. OIDC, presencia y límites por usuario están implementados; la restauración de backups en el homelab real y el empaquetado siguen pendientes: **no presentar este estado como listo para documentos reales**.

## Historial

- 2026-11-03 · C4.4 · Diálogos y cinta de colaboración; descubrimiento de comandos, publicación protegida, validación sin reemplazar documentos, títulos Unicode y cierre cancelable del transporte. Próximo trabajo: permisos/solo lectura desde la conexión inicial (C5.2).
- 2026-11-03 · C5.2 · Capacidades iniciales y en caliente, pertenencia comprobada en todas las rutas implementadas, revocación de suscripciones y resincronización con renovación de permisos. Botones, tooltips y comandos reflejan el modo de solo lectura. Se cierra esta entrega sin iniciar C5.1.
- Validación final C4.4/C5.2: `cargo xtask ci` pasa sus 9 pasos (fmt, clippy, test, i18n-report, language, doctest, assets, layers, wasm); la etapa de tests ejecuta 4692 pruebas correctas, 0 fallidas y 5 ignoradas en 44 binarios. Assets: 48 archivos atribuidos; capas: 23 crates; wasm: 18 crates. Se verifican dos editores reales, un lector desde el primer snapshot, cambios de rol y revocación HTTP/WebSocket. La primera CI había detectado un fixture de paginación dependiente de la fuente del sistema: se reprodujo aisladamente y se estabilizó con la fuente ya incluida y comprobaciones físicas intactas, sin cambiar la maquetación de producción. También se corrigió la traducción del hint del servidor que detectó la auditoría de interfaz. Capturas `ui_shot` inspeccionadas: `/tmp/opencode/wordcraft-share-*.png` (cinta, formularios, claro/oscuro/RTL) y `/tmp/opencode/wordcraft-c5.2-readonly-*.png` (lector, botones deshabilitados y edición rechazada). El estado final no declara aptitud para producción ni inicia C5.1.
- 2026-11-03 · C6.1 · Test de caos con tres clientes reales detrás de un proxy WebSocket que corta conexiones y duplica mensajes, y dos reinicios del servidor. Comprueba convergencia con un cliente que entra tarde desde disco, registro sin duplicados, enviadas = confirmadas + rechazadas, y solo conflictos como rechazos. Ronda típica: 240 ediciones, unos 55 cortes y 140 duplicados, de 0 a 7 conflictos. El test reveló tres bugs, ya corregidos con tests de regresión. Uno: el editor enviaba las ediciones con la revisión del transporte en vez de la que mostraba; con edición concurrente daba 14 rechazos `InvalidOperation` por ronda. Dos: un id rechazado podía confirmarse al reenviarse. Tres: tras reiniciar el servidor, las ediciones pendientes de un cliente rezagado se rechazaban con `StaleRevision` y se perdían, porque el historial empezaba en el snapshot de cierre (salió en la CI completa, bajo carga). Validación: 20 rondas en 4 procesos paralelos, todas correctas. Próximo trabajo: C6.2.
- 2026-11-03 · C5.5 · Pantalla de participantes: lista de acceso, invitar buscando por nombre, cambiar el rol y quitar el acceso desde el editor, sin `curl`. Tres endpoints nuevos (`members`, `users?q=`, `me`).
- 2026-11-03 · C6.2/C6.4 · Backups cifrados con age (copia en caliente, verificación, restauración, retención), probados en tests y en Docker Compose local. Pings WebSocket a medias. Plan de C6.5 (enlaces para compartir, Cloudflare Tunnel). Trabajo parado a petición del propietario; estado en «Dónde seguir».
- 2026-11-04 · C6.2/C6.4 · Desarrollo retomado desde el estado anterior sin modificar `data/`. Corregido Clippy del keepalive, pánico de intervalos Unicode, SQLite temporal sin cifrar, límites de extracción y cierre bloqueado por backpressure. Pruebas focalizadas: servidor (22 unitarias + 1 de intervalos CLI + 4 integración) y transporte (6) correctas; keepalive repetido 20/20 veces. Prueba de CLI en un directorio aislado: crear usuario → backup → `verify-backup latest --identity -` → `restore --identity -`, usuario conservado; restaurar sobre datos existentes e intervalo `ñ` rechazados sin pánico. Compose valida con `config --quiet`, sin arrancar servicios. CI completa en curso; no se inicia C6.5 antes de validarla.
- 2026-11-04 · C6.4 · El test de caos descubrió que cerrar solo `Client::events` no cancela nada cuando su receptor se ha trasladado a una tarea de consumo. `close` cancela ahora la propia tarea de transporte; regresión específica con consumidor separado. Tras la corrección pasan 3 rondas: 720 ediciones, 706 confirmadas y 14 conflictos, 172 cortes y 399 duplicados, sin pérdidas. Se interrumpió únicamente el proceso de test bloqueado de la CI anterior y se relanzó la puerta completa. Contrato de C6.5 preparado en [`share-links.md`](share-links.md).
- 2026-11-04 · C6.2/C6.4 · Puerta final en verde: 4735 pruebas correctas, 0 fallidas, 5 ignoradas, 48 binarios; pasan fmt, Clippy, test, i18n-report, language, doctest, assets, layers y wasm. Se cierra la validación local; la restauración en el homelab real no se da por hecha.
- 2026-11-04 · C6.5a/C6.5c · API de invitaciones con sesiones de invitado acotadas al documento; 9 pruebas de integración y Clippy del servidor correctos. Compose alternativo Cloudflare validado sin secretos ni servicios arrancados; sus servicios de aplicación/backup son idénticos al stack anterior. CI completa en curso; UI pendiente.
- 2026-11-04 · C6.5a/C6.5c · Validación final: 4744 pruebas correctas, 0 fallidas, 5 ignoradas en 49 binarios, los 9 pasos de `cargo xtask ci` correctos. Servidor de invitaciones y configuración de despliegue listos para integrar por PR; quedan UI, sesión guardada y prueba real del túnel/homelab.
- 2026-11-04 · Plan C7 · El propietario prioriza formato básico y copiar/pegar como siguiente entrega, por delante de C6.5b. Se concretan texto Unicode, varios párrafos, reemplazo de selección, portapapeles del sistema, permisos, formato admitido, compatibilidad de protocolo y pruebas de concurrencia/durabilidad. Se documenta el alojamiento central y la publicación de archivos existentes; no se presentan estas funciones de edición nuevas como implementadas.
- 2026-11-04 · Plan C7 ampliado · Se incluye la pantalla para listar, buscar y abrir documentos accesibles del servidor sin copiar IDs. El propietario precisa «solo plantea»: solo queda documentación modificada en el árbol, sin pruebas ejecutadas. A petición posterior, el inicio parcial de código queda apartado en el stash local `f445bf02682d4a55c582e6738a61253105cf78b6`, incompleto y sin validar. El futuro desarrollo completará C7.1–C7.4 antes de la validación final C7.5.
- 2026-11-04 · C7.1–C7.4 · Desarrollo autorizado por el propietario, con las pruebas al final (C7.5). Implementados lote atómico y formato en el protocolo v2, pegado/corte/formato compartidos y listado de documentos en *Open Shared*. Compilan la app y el servidor y Clippy está limpio en los crates tocados; validación completa pendiente.
- 2026-11-04 · C7.5 · Pruebas de C7 añadidas y `cargo xtask ci` completo en verde (9 pasos; 4763 correctas, 0 fallidas, 5 ignoradas). Bloque C7 validado en local; pendientes la prueba en el homelab y el PR.
- 2026-11-04 · C6.5b · Interfaz de enlaces (crear/copiar/revocar en Participants, abrir en *Open Shared* como cuenta o invitado) y sesión de invitado en el almacén del sistema. CI completa en verde: 9 pasos, 4765 correctas, 0 fallidas, 5 ignoradas. El propietario autoriza publicar rama e imagen para probar en el homelab.
- 2026-11-04 · Publicación · Rama `collab/c7-shared-editing` (C7 + C6.5b, `849fad0e`) subida a `origin`; imagen `ghcr.io/iisra-dev/wordcraft-collab-server:849fad0e` publicada (linux/amd64, digest `sha256:24632b27…`), probada en local con `health/ready`. Protocolo v2: servidor y editores se actualizan juntos; `3bc2fd2b` sigue disponible para volver atrás. Pendiente: prueba en el homelab y PR a `main`.
