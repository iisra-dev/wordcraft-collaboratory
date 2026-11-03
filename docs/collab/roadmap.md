# WordCraft Collaborative: hoja de ruta y estado

> **Última actualización:** 2026-11-03 · **Rama:** `collab/c6.1-chaos` (desde `collab/scaffold`) · **Siguiente tarea:** C6.2 (imagen Docker en el homelab, backups cifrados y restauración) · Diseño: [`design.md`](design.md) · Gestión del fork: [`fork.md`](fork.md)

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
| C6 · Estabilización | 🟡 C6.1 hecho; Dockerfile + Compose iniciales |

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
- **C6.2** Imagen Docker verificada en el homelab; backups cifrados y restauración probada.
- **C6.3** Empaquetado del cliente para Linux, Windows y macOS.

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

El token no se guarda en preferencias y debe volver a introducirse tras reiniciar. Los comandos `ui.collab.connect` / `publish` siguen siendo programáticos: no abren diálogos. La interfaz web no tiene transporte colaborativo. Solo se comparten las ediciones de texto y párrafos admitidas; no formato ni undo/redo. El lector ve **Shared · read-only** desde el primer snapshot; promocionarlo/degradarlo actualiza las capacidades en caliente. Tras revocar y volver a conceder lectura debe abrir o resincronizar el documento de nuevo. OIDC, presencia, límites por usuario y backups cifrados siguen pendientes: **no presentar este estado como listo para documentos reales**.

## Historial

- 2026-11-03 · C4.4 · Diálogos y cinta de colaboración; descubrimiento de comandos, publicación protegida, validación sin reemplazar documentos, títulos Unicode y cierre cancelable del transporte. Próximo trabajo: permisos/solo lectura desde la conexión inicial (C5.2).
- 2026-11-03 · C5.2 · Capacidades iniciales y en caliente, pertenencia comprobada en todas las rutas implementadas, revocación de suscripciones y resincronización con renovación de permisos. Botones, tooltips y comandos reflejan el modo de solo lectura. Se cierra esta entrega sin iniciar C5.1.
- Validación final C4.4/C5.2: `cargo xtask ci` pasa sus 9 pasos (fmt, clippy, test, i18n-report, language, doctest, assets, layers, wasm); la etapa de tests ejecuta 4692 pruebas correctas, 0 fallidas y 5 ignoradas en 44 binarios. Assets: 48 archivos atribuidos; capas: 23 crates; wasm: 18 crates. Se verifican dos editores reales, un lector desde el primer snapshot, cambios de rol y revocación HTTP/WebSocket. La primera CI había detectado un fixture de paginación dependiente de la fuente del sistema: se reprodujo aisladamente y se estabilizó con la fuente ya incluida y comprobaciones físicas intactas, sin cambiar la maquetación de producción. También se corrigió la traducción del hint del servidor que detectó la auditoría de interfaz. Capturas `ui_shot` inspeccionadas: `/tmp/opencode/wordcraft-share-*.png` (cinta, formularios, claro/oscuro/RTL) y `/tmp/opencode/wordcraft-c5.2-readonly-*.png` (lector, botones deshabilitados y edición rechazada). El estado final no declara aptitud para producción ni inicia C5.1.
- 2026-11-03 · C6.1 · Test de caos con tres clientes reales detrás de un proxy WebSocket que corta conexiones y duplica mensajes, y dos reinicios del servidor. Comprueba convergencia con un cliente que entra tarde desde disco, registro sin duplicados, enviadas = confirmadas + rechazadas, y solo conflictos como rechazos. Ronda típica: 240 ediciones, unos 55 cortes y 140 duplicados, de 0 a 7 conflictos. El test reveló tres bugs, ya corregidos con tests de regresión. Uno: el editor enviaba las ediciones con la revisión del transporte en vez de la que mostraba; con edición concurrente daba 14 rechazos `InvalidOperation` por ronda. Dos: un id rechazado podía confirmarse al reenviarse. Tres: tras reiniciar el servidor, las ediciones pendientes de un cliente rezagado se rechazaban con `StaleRevision` y se perdían, porque el historial empezaba en el snapshot de cierre (salió en la CI completa, bajo carga). Validación: 20 rondas en 4 procesos paralelos, todas correctas. Próximo trabajo: C6.2.
- 2026-11-03 · C5.5 · Pantalla de participantes: lista de acceso, invitar buscando por nombre, cambiar el rol y quitar el acceso desde el editor, sin `curl`. Tres endpoints nuevos (`members`, `users?q=`, `me`).
