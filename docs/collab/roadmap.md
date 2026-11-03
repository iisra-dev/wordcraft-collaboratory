# WordCraft Collaborative: hoja de ruta y estado

> **Última actualización:** 2026-11-03 · **Rama:** `collab/scaffold` · **Siguiente tarea:** C2.3 · Diseño: [`design.md`](design.md)

Los ids de tarea (`C1.2`…) van en los mensajes de commit, como pide `AGENTS.md` (`C1.2: apply InsertText/DeleteText`). Antes de cada commit: `cargo xtask ci`.

## Estado

| Hito | Estado |
| --- | --- |
| C0 · Preparación del repositorio | ✅ hecho |
| C1 · Modelo colaborativo | ✅ hecho |
| C2 · Servidor autoritativo | 🟡 en curso (C2.1, C2.2 y C2.4 hechos) |
| C3 · Cliente nativo básico | 🟡 esqueleto (estado, cola de pendientes) |
| C4 · Integración con el motor | — |
| C5 · Seguridad y experiencia | — |
| C6 · Estabilización | 🟡 Dockerfile + Compose iniciales |

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
| Las ediciones remotas no generan revisiones de control de cambios | Pendiente de C4: el motor decide si marcar la edición local antes de convertirla en operación |

## C1 · Modelo colaborativo (hecho)

Objetivo (diseño §16): dos documentos independientes que reciben las mismas operaciones terminan idénticos, sin red.

- ✅ **C1.1** Registro de identidades completo: asignación inicial desde un `Document`, serialización en el snapshot colaborativo (documento + ids), restauración validada. `SharedDocument` (`collab-model/src/snapshot.rs`): `{format, document, blocks}` en JSON, formato versionado (`SNAPSHOT_FORMAT`), campos desconocidos rechazados, registro comprobado contra el cuerpo (número de bloques, ids únicos con un conjunto, sin coste cuadrático) y runs normalizados como al abrir JSON nativo.
- ✅ **C1.2** `apply`: `InsertText` y `DeleteText` sobre `wordcraft_doc` (`insert_text`, `delete_range`), validando rango, límites de carácter y objetos incrustados; sin cambios si hay error. Propiedad `proptest`: dos réplicas con las mismas operaciones quedan idénticas y un error no cambia nada.
- ✅ **C1.3** `transform` insert/insert en el mismo bloque con desempate determinista (diseño §5.3), luego insert/delete y delete/delete. Cada regla con tests unitarios y `proptest`. Tabla de reglas en `collab-model/src/transform.rs`; propiedad TP1 (ambos órdenes del servidor convergen salvo el empate, que respeta el orden del servidor) y ninguna inserción se pierde.
- ✅ **C1.4** `SplitParagraph` y `MergeParagraphs` en `apply` (actualizando el registro) y sus transformaciones, o `Conflict` documentado. `MergeParagraphs` lleva `first_len`; las operaciones estructurales son atómicas (copia de documento y registro); propiedad TP1 con dos párrafos, divisiones y fusiones.
- ✅ **C1.5** Prueba de convergencia: N réplicas + "servidor" en memoria con secuencias aleatorias (`proptest`), incluido Unicode multibyte. Propiedad: `D_A = D_B = D_servidor`. `collab-model/tests/convergence.rs`: 3 clientes que editan sobre réplicas desfasadas y un cuarto que entra tarde desde un snapshot; documento y registro idénticos al servidor; tras transformar solo se admite `Conflict` como rechazo. Ejecución larga fija: 2469 commits y 48 conflictos en 3000 pasos.
- ✅ **C1.6** Lo mismo con dos `Session` reales de `wordcraft-engine` (prueba de concepto del §16). `collab-client/tests/sessions.rs` (el motor es solo dependencia de desarrollo): dos sesiones entran desde un snapshot, reciben los commits en `Session::doc` sin `set_document`, siguen maquetando y quedan idénticas al servidor; después, los mismos comandos reales dan el mismo documento. Con el documento de muestra (22 bloques, una tabla): 313 commits en 400 pasos.

## C2 · Servidor autoritativo

- ✅ **C2.1** Repositorio SQLite: usuarios, documentos, miembros, operaciones (inserción transaccional, unicidad por `operation_id`). `persistence/repository.rs`: `commit_operation` escribe la revisión siguiente y mueve `current_revision` en una transacción, exige la revisión esperada por el actor y devuelve el commit original ante un `operation_id` repetido; un único propietario por documento; textos de usuario validados; filas corruptas dan error, nunca pánico. Migración `0002` (`client_id` por operación, índice de miembros por usuario).
- ✅ **C2.2** `DocumentActor`: canal Tokio, copia de trabajo, persistir antes de publicar, idempotencia. `documents/actor.rs` + `tests/actor.rs`: inserciones concurrentes que convergen y llegan a todos (autor y lectores incluidos), reenvío aplicado una vez, rechazos sin cambios (lector, ajeno, revisión futura, offset inválido, conflicto), reinicio que conserva revisión y contenido, participante lento descartado y `resync`, fallo de almacenamiento sin cambios y log incoherente rechazado al arrancar.
- **C2.3** Endpoint `/api/v1/ws`: `hello`/`join`/`submit`/`resync`/`presence`, límites de tamaño, backpressure.
- ✅ **C2.4** Snapshots atómicos con SHA-256; recuperación = último snapshot válido + operaciones posteriores; test de reinicio. Hecho antes que C2.3, porque el endpoint necesita abrir documentos. `storage/mod.rs` (temporal → fsync → rename → fsync del directorio; claves validadas; lectura acotada), `documents::open` (el snapshot más reciente que se lee íntegro y encaja con el log; si no, uno anterior), `documents::create` (snapshot de revisión 0 + filas en una transacción). El actor guarda cada 100 commits y al pararse. `tests/recovery.rs`.
- **C2.5** API HTTP de documentos (crear/importar DOCX, listar, metadatos, exportar, historial, miembros).

## C3 · Cliente nativo básico

- **C3.1** Transporte WebSocket nativo tras la feature `transport` (tokio-tungstenite + rustls), fuera del build wasm.
- **C3.2** Coordinador: `join` → snapshot → `CollaborationState::Synchronized`; aplicar `committed`.
- **C3.3** Reconexión: `resync` desde la última revisión confirmada y reenvío de `PendingQueue`.

## C4 · Integración con el motor

- **C4.1** Captura `CommittedEdit` en el límite de `Session::run` para los comandos admitidos (escritura, Retroceso/Supr, Intro).
- **C4.2** `apply_remote_edit` sin `set_document`: identidades, selección/anclajes, invalidación de layout, sin evento saliente.
- **C4.3** Bloquear comandos no admitidos y undo/redo en documentos compartidos (con motivo visible en la UI).
- **C4.4** Comandos y UI: conectar, abrir/publicar documento compartido, estado de sincronización siempre visible.

## C5 · Seguridad y experiencia

- **C5.1** OIDC Authorization Code + PKCE con Authentik (navegador del sistema, callback loopback), validación de tokens en el servidor, token en el gestor de credenciales del SO.
- **C5.2** Permisos por documento en todas las operaciones y endpoints; viewers en solo lectura.
- **C5.3** Presencia: participantes y cursores remotos con frecuencia limitada.
- **C5.4** Mensajes de error y conflicto comprensibles; límites por conexión/usuario; auditoría.

## C6 · Estabilización

- **C6.1** Tests de integración con tres clientes, desconexiones aleatorias, duplicados y reinicios.
- **C6.2** Imagen Docker verificada en el homelab; backups cifrados y restauración probada.
- **C6.3** Empaquetado del cliente para Linux, Windows y macOS.

## Cómo trabajar

```bash
# tests de los crates de colaboración
cargo test -p wordcraft-collab-protocol -p wordcraft-collab-model -p wordcraft-collab-client -p wordcraft-collab-server
# servidor local (datos en ./data)
cargo run -p wordcraft-collab-server
curl localhost:3000/api/v1/health/ready
# antes de commitear
cargo xtask ci
```

Sincronizar con upstream: `git fetch upstream && git merge upstream/main`. Los cambios en crates existentes (`engine`, `ui-egui`) se mantienen pequeños y localizados para que esos merges sigan siendo fáciles.
