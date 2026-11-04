# WordCraft Collaborative: diseño técnico de implementación

> **Estado:** arquitectura v0.1, MVP implementado con estabilización pendiente · **Última actualización:** 2026-11-04 · Plan de trabajo y estado: [`roadmap.md`](roadmap.md)

> **Implementación:** el MVP comunica permisos iniciales y cambios en caliente, serializa los cambios de miembros con las ediciones y retira las suscripciones revocadas. OIDC, presencia visual y límites por usuario están implementados (C5); backups y keepalive pasan sus pruebas focalizadas (C6.2/C6.4). La restauración en el homelab real y el empaquetado siguen pendientes; no se declara apto para producción.

**Objetivo:** que varios usuarios ejecuten WordCraft en sus propios ordenadores y editen documentos compartidos en tiempo real, con un servidor central en un homelab, sin depender de servicios cloud.

Linux, Windows y macOS, con el cliente de escritorio como prioridad. La interfaz web queda fuera del MVP.

## 1. Decisiones arquitectónicas

| Aspecto | Decisión |
| --- | --- |
| Lenguaje | Rust |
| Cliente | WordCraft nativo, basado en egui |
| Servidor | Axum + Tokio |
| Comunicación | WebSocket sobre TLS |
| Sincronización inicial | Operaciones secuenciadas por el servidor |
| Gestión de conflictos | Transformación/rebase de operaciones compatibles |
| Base de datos | SQLite con WAL |
| Almacenamiento | Sistema de archivos del homelab |
| Autenticación | OIDC con Authentik, opcionalmente autenticación local |
| Despliegue | Docker Compose |
| Edición offline | No incluida en v0.1 |
| Formato de exportación | DOCX |
| Arquitectura futura | Compatible con evolución hacia CRDT |

La prioridad es un sistema de colaboración fiable, mantenible y con recuperación ante fallos, antes que todas las funciones avanzadas de edición concurrente.

### Restricciones del MVP

Varios usuarios podrán editar simultáneamente párrafos distintos y hacer cambios compatibles sobre el mismo párrafo. Las operaciones estructurales complejas (tablas, secciones) quedan bloqueadas en documentos compartidos hasta tener una semántica de concurrencia definida. Así no se presenta como segura una sincronización que aún no garantiza la integridad de documentos complejos.

## 2. Encaje con WordCraft

WordCraft usa Rust 2024 (mínimo 1.95), `wordcraft-doc` como modelo de documentos, `wordcraft-engine` como motor de comandos, edición e historial, `wordcraft-ui-egui` como interfaz y `apps/wordcraft` como aplicación nativa. El workspace incluye `crates/*` y `apps/*`, así que se añaden componentes sin tocar el sistema de compilación. Las capas las impone `cargo xtask layers` (ver `AGENTS.md`).

Cuatro particularidades condicionan toda la implementación:

1. **El documento no es texto plano.** `Document` contiene bloques, estilos, numeración, comentarios, revisiones, imágenes y otros objetos.
2. **Las posiciones son estructurales.** Un `Pos` identifica una historia, una ruta (`Path`) hasta un párrafo y un desplazamiento en bytes. No sirve como identificador estable entre clientes: al insertar o eliminar bloques, las rutas cambian.
3. **Las acciones pasan por `Session::run`** (`crates/engine/src/session/run.rs`), que ejecuta comandos y gestiona snapshots, errores y estado de edición. Es el mejor punto de instrumentación, pero no todos sus comandos son operaciones reproducibles de colaboración.
4. **El undo/redo es local**, integrado en la sesión. Hay que cambiarlo para que un usuario no deshaga los cambios de otro.

## 3. Arquitectura de componentes

La pieza central es un `DocumentActor`: un proceso lógico por documento abierto que recibe operaciones y las serializa. Cada documento tiene una única secuencia autoritativa de revisiones; documentos distintos se procesan concurrentemente.

Flujo de edición:

1. **El usuario escribe.** WordCraft genera un cambio a través del motor.
2. **El cliente prepara una operación** con identificadores estables.
3. **El servidor valida** permisos, revisión base y compatibilidad con otros cambios.
4. **El servidor confirma**: persiste la operación y asigna una nueva revisión.
5. **Los clientes aplican el cambio** y recalculan las partes afectadas de la interfaz.

El servidor persiste la operación **antes** de confirmarla: una desconexión posterior no puede perder un cambio que el cliente cree guardado.

## 4. Estructura del proyecto

El código de red no entra en `wordcraft-engine`. Crates independientes y un adaptador entre el motor y la colaboración:

```
wordcraft/
├── crates/
│   ├── doc/                        # existente
│   ├── engine/                     # existente, se modificará
│   ├── ui-egui/                    # existente, se modificará
│   ├── collab-protocol/            # NUEVO: ids, operaciones, mensajes, errores
│   ├── collab-model/               # NUEVO: identidades, apply, transform, anchors
│   └── collab-client/              # NUEVO: connection, coordinator, pending, presence
├── apps/
│   ├── wordcraft/                  # app de escritorio existente
│   └── wordcraft-collab-server/    # NUEVO: config, auth, api, ws, documents, persistence, storage
│       ├── migrations/
│       └── Dockerfile
└── deploy/
    ├── compose.yaml
    ├── Caddyfile
    └── .env.example
```

| Crate | Capa | Responsabilidad |
| --- | --- | --- |
| `collab-protocol` | L0 standalone | Tipos de mensajes, ids, versiones y serialización |
| `collab-model` | L2 | Operaciones sobre documentos, identidades, validación y transformaciones |
| `collab-client` | L5 | Conexión, reconexión, estado remoto y cola local |
| `collab-server` | app (exenta) | Autenticación, permisos, sesiones, persistencia y almacenamiento |

**Regla fundamental:** `collab-model` no depende de Tokio, WebSocket, egui ni SQLite; solo de las estructuras documentales. La corrección de las operaciones se prueba sin servidores ni interfaces gráficas. `collab-protocol`, `collab-model` y `collab-client` compilan para wasm (`cargo xtask wasm`); el transporte nativo del cliente irá tras una feature no activada por defecto.

## 5. Modelo de operaciones colaborativas

Aquí va el mayor esfuerzo de diseño.

### 5.1. Identificadores estables

Las rutas sirven para localizar párrafos en una estructura concreta, no para identificarlos durante toda su vida. Tipos (en `collab-protocol/src/ids.rs`): `BlockId(Uuid)`, `DocumentId(Uuid)`, `OperationId(Uuid)`, `ClientId` (validado) y `Revision = u64`.

El servidor mantiene la correspondencia `BlockId` ↔ ruta de WordCraft en un **registro paralelo de identidades** (`collab-model/src/identity.rs`), persistido en los snapshots colaborativos, para no modificar el formato documental existente. El registro se actualiza **explícitamente** al insertar, dividir, combinar o eliminar párrafos; nunca se reconstruye solo a partir de índices, porque un párrafo puede desplazarse.

### 5.2. Operaciones admitidas

```rust
pub enum DocumentOperation {
    InsertText { block_id: BlockId, offset: u32, text: String },
    DeleteText { block_id: BlockId, start: u32, end: u32 },
    SplitParagraph { block_id: BlockId, offset: u32, new_block_id: BlockId },
    MergeParagraphs { first: BlockId, second: BlockId, first_len: u32 },
}
```

`first_len` es la longitud en bytes de `first` antes de combinar (donde empieza el texto de `second` después): permite transformar ediciones concurrentes en `second` sin leer el documento y hace la operación invertible para el undo colaborativo. `apply` comprueba que coincide con el documento.

Los desplazamientos son **offsets de bytes UTF-8**, como `Pos` en WordCraft. El servidor comprueba que caen en límites de carácter válidos y respetan las reglas de los objetos incrustados. Nunca índices UTF-16 ni posiciones visuales: son representaciones distintas y mezclarlas corrompe Unicode.

Las operaciones de formato vendrán después, con semántica explícita para los conflictos entre estilos.

### 5.3. Transformación de operaciones concurrentes

Dos clientes en la revisión 10 con `Hello world`: A inserta `beautiful ` en el offset 6, B inserta `!` en el offset 11. Resultado tras integrar ambas: `Hello beautiful world!`. Si el servidor recibe primero la de A, desplaza el offset de B:

```
p_B' = p_B + |t_A|_bytes   si p_A < p_B
p_B' = p_B                 si p_A > p_B
```

Inserciones en la misma posición necesitan un desempate determinista (p. ej. por id de operación y de cliente). También hacen falta reglas para inserción/borrado, borrados superpuestos y operaciones estructurales.

**Regla de seguridad:** si no existe una transformación implementada y probada para una combinación de operaciones, el servidor rechaza la segunda con `Conflict` en lugar de aplicarla en una posición potencialmente incorrecta.

### 5.4. No enviar comandos de WordCraft

No se transmiten mensajes como `{"command": "text.insert", "params": {"text": "Hello"}}`: el comando usa la selección y otro estado del cliente que lo ejecuta, y repetido en otra máquina modificaría otra posición. **Se envía el efecto documental, no la intención de la interfaz.**

Captura de cambios en el límite del motor:

```rust
pub struct CommittedEdit {
    pub operations: Vec<DocumentOperation>,
    pub selection_after: SelectionAnchor,
}
```

La captura se limita a los comandos admitidos. Los demás siguen funcionando en documentos locales, pero la interfaz impide ejecutarlos sobre un documento compartido mientras no haya un adaptador seguro. Un diff genérico de dos documentos no es el mecanismo definitivo: detectar inserciones y borrados no preserva la intención estructural de una edición.

## 6. Integración con `Session`

Se modifica la capa que llama a `Session::run` para introducir un coordinador colaborativo; el comando original sigue siendo la autoridad sobre el comportamiento local del editor.

```rust
pub struct CollaborativeSession {
    pub session: Session,
    pub collaboration: CollaborationState,
}
```

`CollaborationState` (en `collab-client/src/coordinator.rs`): `Local`, `Connecting`, `Synchronized`, `Reconnecting`, `Conflict`, `ReadOnly`. El estado real guardará además operaciones pendientes, identidades, capacidades y datos de reconexión.

### Política de aplicación de cambios (MVP)

**Confirmación antes de aplicación visible:**

1. El cliente convierte el comando admitido en una operación.
2. La envía al servidor.
3. El servidor la valida, transforma si procede y confirma.
4. El cliente aplica la operación confirmada al documento local.
5. El motor recalcula el diseño y actualiza la pantalla.

Introduce latencia perceptible, pero evita la complejidad de ediciones optimistas pendientes mientras llegan cambios remotos. La vista especulativa local (rebase y rollback) llegará después de probar exhaustivamente la convergencia.

### Cambios remotos

```rust
pub fn apply_remote_edit(session: &mut Session, operation: &DocumentOperation) -> Result<(), CollaborationError>;
```

Debe: aplicar la modificación al modelo documental; actualizar las identidades de bloques; transformar la selección y los anclajes locales; invalidar la caché de disposición afectada; actualizar la revisión compartida; no generar un evento saliente; mantener el historial remoto separado del undo local.

**No** se usa `Session::set_document` por edición: sustituye el documento y reinicia el historial, entre otras cosas.

### Undo y redo

En el MVP colaborativo el undo/redo tradicional se desactiva sobre documentos compartidos. Después, cada operación del usuario guardará lo necesario para construir su inversa, que se transformará frente a las operaciones confirmadas posteriormente: deshacer será una nueva operación compartida, no restaurar un snapshot local.

## 7. Protocolo de red

Protocolo versionado sobre WebSocket, JSON en el MVP. Implementado en `collab-protocol/src/messages.rs`.

| Mensaje | Dirección | Función |
| --- | --- | --- |
| `hello` / `welcome` | C → S / S → C | Negociación de versión |
| `join` | C → S | Entrar en un documento |
| `snapshot` | S → C | Estado inicial |
| `submit` | C → S | Enviar una operación |
| `committed` | S → clientes | Publicar una operación confirmada |
| `rejected` | S → C | Error o conflicto |
| `presence` | bidireccional | Cursores y participantes |
| `permissions` | S → C | Capacidad actual por destinatario (`can_read`, `can_edit`); una revocación termina su suscripción |
| `resync` | C → S | Recuperar cambios perdidos |

Cada operación lleva `operation_id`, `client_id`, `document_id` y `base_revision`; las confirmaciones incluyen la revisión asignada.

El transporte nativo envía tramas Ping cada 25 s y reconecta tras tres intervalos sin recibir nada, para detectar conexiones muertas y mantenerlas abiertas tras proxies (C6.4). El cierre cancela también un handshake, escritura o envío a la cola del editor bloqueados; la configuración de los intervalos se valida antes de abrir la conexión.

Desde C5.2 cada snapshot lleva `can_edit`, calculado por el servidor para ese usuario y no guardado dentro del documento. Un cliente actualizado interpreta un campo ausente como `false` (servidores anteriores: solo lectura). `permissions` actualiza el permiso sin cambiar la revisión documental. El servidor sigue autorizando cada operación aunque el cliente ignore la capacidad. Las ampliaciones son aditivas al protocolo v1.

```json
{
  "protocol_version": 1,
  "type": "submit",
  "document_id": "0192f7a0-0000-7000-8000-000000000001",
  "operation_id": "0192f7a1-0000-7000-8000-000000000002",
  "client_id": "desktop-a",
  "base_revision": 42,
  "operation": {
    "kind": "insert_text",
    "block_id": "0192f7a2-0000-7000-8000-000000000003",
    "offset": 6,
    "text": "collaborative "
  }
}
```

(Este ejemplo es un test: `messages::tests::design_example_round_trips`.)

### Reconexión e idempotencia

Si el servidor acepta una operación y la conexión se pierde antes de que el cliente reciba la confirmación, al reconectar:

1. El cliente envía su última revisión confirmada.
2. El servidor devuelve las operaciones posteriores, o un snapshot si ya no tiene el historial necesario.
3. El cliente reenvía las operaciones sin confirmar.
4. El servidor detecta los `operation_id` ya procesados y devuelve su confirmación original sin ejecutarlos otra vez.

Un `operation_id` tiene un solo veredicto (C6.1). Si una operación se rechaza por sí misma (`Conflict`, `InvalidOperation`, `UnsupportedOperation`), el actor guarda ese rechazo en memoria (los últimos 4096 por documento abierto). Un reenvío del mismo id recibe el mismo rechazo, aunque ahora cupiera en el documento: así nunca se confirma una edición de la que ya se dijo al autor que se había rechazado. Los rechazos que pueden cambiar (permisos, servidor no disponible, revisión base por delante) no se guardan. Un reinicio olvida estos rechazos. Un reenvío posterior a un reinicio solo puede venir de un cliente que no llegó a recibir la respuesta, así que evaluarlo de nuevo es correcto.

La revisión base es la del documento que el autor estaba viendo. El hilo de la interfaz captura la edición sobre su sesión, y el hilo de transporte puede haber aplicado ya confirmaciones posteriores. Por eso `submit_edit` lleva la revisión que muestra el editor, y el servidor transforma la edición sobre todo lo confirmado desde entonces. Si se usara la revisión del transporte, los offsets se interpretarían contra un texto que el autor no había visto: la edición acabaría desplazada o rechazada como inválida (C6.1).

Al reiniciarse, el actor recupera su historial en memoria (hasta `MAX_HISTORY` confirmaciones) desde el registro de operaciones, incluidas las anteriores al snapshot del que arranca. El actor siempre escribe un snapshot en la revisión más reciente al pararse. Si el historial empezara en ese snapshot, cualquier edición pendiente de un cliente que iba por detrás se rechazaría con `StaleRevision` al reenviarse tras el reinicio, y se perdería (C6.1).

La tabla de operaciones tiene unicidad por (documento, id de operación). El orden de llegada de WebSocket no sustituye a las transacciones ni a la recuperación.

En la implementación C5.2, `resync` WebSocket hace un nuevo `join` atómico y devuelve un snapshot: renueva tanto el contenido como la suscripción y las capacidades. El forwarder anterior se cancela antes del snapshot; todas las respuestas y eventos pasan por la misma cola FIFO, evitando que un permiso antiguo llegue después de un snapshot más nuevo. Las consultas internas al actor todavía pueden reproducir el historial reciente.

## 8. Servidor de colaboración

### DocumentActor

Un actor lógico por documento con un canal Tokio:

```rust
pub struct DocumentActor {
    document_id: DocumentId,
    revision: Revision,
    document: Document,
    identities: BlockIdentityMap,
    subscribers: Vec<Subscriber>,
    repository: DocumentRepository,
}
```

Ciclo: recibir → autenticar y autorizar → validar operación y revisión base → transformar frente a las confirmadas → aplicar al estado de trabajo → persistir operación y nueva revisión → publicar `committed`.

El cambio se aplica a una copia de trabajo (o con rollback seguro); el estado autoritativo solo cambia tras completar la persistencia. El procesamiento secuencial por documento basta para un homelab con pocos participantes.

### API HTTP

| Método | Endpoint | Función |
| --- | --- | --- |
| `GET` | `/api/v1/documents` | Listar documentos accesibles |
| `POST` | `/api/v1/documents` | Crear o importar |
| `GET` | `/api/v1/documents/{id}` | Metadatos |
| `GET` | `/api/v1/documents/{id}/export` | Descargar DOCX |
| `POST` | `/api/v1/documents/{id}/snapshots` | Crear versión |
| `GET` | `/api/v1/documents/{id}/history` | Historial |
| `GET` | `/api/v1/documents/{id}/members` | Quién tiene acceso (cualquier miembro; el propietario primero) |
| `PUT` | `/api/v1/documents/{id}/members/{user_id}` | Actualizar permisos |
| `GET` | `/api/v1/users?q=` | Buscar personas para invitar: nombre que contiene `q` (2–100 caracteres) o su id; solo nombre e id, máx. 20 (C5.5) |
| `GET` | `/api/v1/me` | Quién es el titular del token |
| `GET` | `/api/v1/health/live` | Comprobación de vida |
| `GET` | `/api/v1/health/ready` | Comprobación de disponibilidad |
| `GET` | `/api/v1/ws` | Conexión WebSocket |

Administración y exportación comprueban permisos **por documento**, no solo que haya sesión iniciada.

C5.2 ordena los cambios de miembros con las operaciones dentro del actor; únicamente el propietario puede gestionarlos y su propiedad no se transfiere ni se elimina por estas rutas. La autorización de escritura y su commit comparten el bloqueo de SQLite. Exportar requiere una lectura autorizada del actor; el historial comprueba pertenencia bajo el bloqueo de su consulta. Al revocar, el actor notifica y elimina las suscripciones: no se entregan commits posteriores, aunque no se pueden retirar copias ya recibidas o mensajes autorizados que estaban en tránsito.

## 9. Persistencia (SQLite)

SQLite en WAL, un único proceso servidor con acceso de escritura. Esquema inicial: `apps/wordcraft-collab-server/migrations/0001_init.sql` (`users`, `documents`, `document_members`, `document_operations`, `document_snapshots`); `0002_operation_client.sql` añade el `client_id` de cada operación (para la idempotencia) y un índice de miembros por usuario; `0003_user_tokens.sql`, los tokens locales (solo su SHA-256); `0004_audit_log.sql`, el registro de auditoría. Acceso: `persistence/repository.rs`. Índices y tablas de invitaciones, auditoría y sesiones llegarán cuando esa API esté definida.

### Estrategia de guardado

| Qué | Cuándo |
| --- | --- |
| Operaciones | Cada cambio confirmado |
| Snapshot interno | Cada 100 operaciones o 5 min |
| Exportación DOCX | Bajo demanda y periódica |
| Backup externo | Diario, configurable |

Valores iniciales, a validar con pruebas de carga.

El formato autoritativo es el documento interno de WordCraft más el registro de identidades y las operaciones. DOCX es importación/exportación, no la fuente de verdad. Los snapshots se escriben de forma atómica, con checksum y referencia persistente; la recuperación toma el último snapshot válido y reproduce las operaciones posteriores. Se conservan los datos OOXML no editados que WordCraft sepa preservar.

Los backups cifrados (C6.2, [`backups.md`](backups.md)) usan la API de backup de SQLite hacia una base de datos en memoria, bajo una transacción de lectura que fija su revisión, más todos los snapshots referidos por esa copia. El zip solo se escribe tras cifrarse con age; ninguna base de datos temporal sin cifrar pasa por la carpeta sincronizada. Verificar restaura en un temporal privado y abre los documentos como el servidor; extraer valida rutas únicas, checksums, tamaños y el presupuesto agregado antes de escribir.

## 10. Interfaz de escritorio

Un apartado de colaboración sin sustituir la interfaz existente: revisión actual, participantes, estado.

Acciones nuevas en el menú del documento: conectar con un servidor; abrir documento compartido; publicar documento local; gestionar participantes; copiar enlace de invitación; consultar historial; desconectar y guardar copia local.

El estado de sincronización es siempre visible: conectado, enviando, sin conexión, solo lectura o conflicto. Las posiciones de otros usuarios viajan como anclajes estructurales con frecuencia limitada; la presencia no es una operación documental y no provoca guardado.

Siguiendo `AGENTS.md`, cada acción será un `CommandSpec` (o un comando `ui.*`), con iconos originales dibujados en código.

## 11. Despliegue en el homelab

Un contenedor con el servidor Rust, un volumen persistente y Caddy como proxy inverso (`deploy/`). Nada de PostgreSQL, Redis ni Kubernetes al principio: añadirían complejidad sin resolver los problemas principales. Caddy gestiona la actualización a WebSocket en `reverse_proxy`.

| Estrategia | Uso recomendado |
| --- | --- |
| Tailscale + acceso privado | Grupo reducido y dispositivos propios |
| WireGuard | Red privada gestionada por ti |
| Dominio + HTTPS + OIDC | Usuarios externos sin VPN |

Para un homelab personal: Tailscale y HTTPS, con autenticación de aplicación igualmente obligatoria. Pertenecer a la VPN no autoriza el acceso a documentos.

## 12. Autenticación y seguridad

OpenID Connect con Authorization Code + PKCE para el cliente de escritorio:

1. El usuario elige «Conectar al servidor».
2. WordCraft abre el navegador del sistema para autenticarse en Authentik.
3. Recibe el resultado en un callback de loopback protegido con `state` y PKCE.
4. Obtiene un token de acceso.
5. Abre la conexión WSS con ese token.
6. El servidor valida emisor, audiencia, firma, expiración y permisos.

Mientras no hay OIDC (C5.1), el servidor ofrece autenticación local: `wordcraft-collab-server create-user <nombre>` crea el usuario y muestra una vez un token aleatorio, del que solo se guarda el SHA-256; el cliente lo envía en `Authorization: Bearer`.

Tokens de sesión de vida corta; nunca credenciales duraderas en URLs ni registros. Los tokens persistentes, si hacen falta, en el gestor de credenciales del sistema operativo.

Obligatorio antes de usarlo con documentos reales:

- Límite de tamaño para mensajes y documentos.
- Control de permisos en todas las operaciones.
- Validación de posiciones y estructuras antes de aplicarlas.
- Límites por conexión y por usuario.
- Backpressure para clientes lentos.
- Auditoría sin almacenar contenido sensible innecesario.
- Protección frente a cargas de archivos maliciosos.
- Backups cifrados y restauración comprobada.
- Prohibido ejecutar comandos de WordCraft recibidos por red.

Los nuevos crates respetan las reglas de `AGENTS.md`: sin `unsafe`, sin rutas de pánico, entradas tratadas como hostiles, y `cargo xtask ci` en verde.

## 13. Pruebas: la parte crítica

Antes de comentarios o cursores de colores, pruebas automatizadas de convergencia y durabilidad.

| Escenario | Resultado requerido |
| --- | --- |
| Dos usuarios insertan en párrafos diferentes | Ambos cambios aparecen |
| Insertan en el mismo offset | Orden determinista |
| Dos borrados se solapan | Resultado correcto o conflicto explícito |
| Un usuario divide un párrafo y otro lo edita | Transformación válida o rechazo |
| El cliente reenvía una operación | Se aplica una sola vez |
| Se pierde la conexión antes del ACK | Se recupera sin duplicar |
| Se reinicia el servidor | Revisión y contenido se conservan |
| Llega una operación no admitida | Se rechaza sin modificar el documento |
| Usuario lector intenta editar | Se deniega |
| El documento contiene Unicode multibyte | No se corrompen los offsets |

Pruebas basadas en propiedades con `proptest` que generan secuencias de operaciones válidas y conflictos. Propiedades principales: `D_A = D_B = D_servidor` tras procesar las mismas operaciones confirmadas, y `revision(n+1) > revision(n)` para cada operación persistida. Se verifica la estructura documental y el contenido, no solo el texto renderizado.

### Integración con varios clientes

Tres clientes de prueba contra un servidor real: join → snapshot; A inserta → commit rev 2 a todos; B inserta → commit rev 3 a todos; se comprueba que el estado documental es idéntico. Después, desconexiones aleatorias, mensajes duplicados y reinicios del servidor. Las selecciones nunca deben quedar apuntando a posiciones inválidas tras una edición remota.

Implementado en `apps/wordcraft-collab-server/tests/chaos.rs` (C6.1). Tres clientes reales editan, cada uno detrás de un proxy WebSocket que corta conexiones sin trama de cierre y duplica `submit`, `committed` y `rejected`. Mientras tanto, el servidor se reinicia dos veces. Las ediciones son aleatorias pero siempre válidas sobre la réplica que leyó el autor: texto ASCII y multibyte, borrados, divisiones y uniones de párrafos. Al final se comprueba que:

- las tres réplicas y un cliente que entra tarde (después de otro reinicio, desde disco) son iguales;
- ningún id aparece dos veces en el registro;
- enviadas = confirmadas + rechazadas;
- los únicos rechazos son conflictos.

La semilla se imprime; `COLLAB_CHAOS_SEED` y `COLLAB_CHAOS_ROUNDS` repiten o alargan la ejecución.

## 14. Orden de desarrollo

Seis entregas comprobables (detalle y estado en [`roadmap.md`](roadmap.md)):

1. **Modelo colaborativo:** identidades estables, operaciones de texto, validaciones y transformaciones.
2. **Servidor autoritativo:** Axum, DocumentActor, SQLite, protocolo versionado y recuperación.
3. **Cliente nativo básico:** conexión, apertura de documento, aplicación de cambios y estado.
4. **Integración con el motor:** captura de ediciones locales, selecciones y comandos admitidos.
5. **Seguridad y experiencia:** OIDC, permisos, presencia, reconexión y mensajes de error.
6. **Estabilización:** tests de concurrencia, durabilidad, empaquetado y despliegue.

### Criterios de MVP terminado

- Dos clientes nativos pueden abrir el mismo documento.
- Las ediciones admitidas aparecen en ambos.
- Los cambios concurrentes compatibles convergen.
- Las operaciones conflictivas se rechazan sin pérdida de datos.
- Un reinicio del servidor no pierde operaciones confirmadas.
- Un usuario sin permisos no puede modificar documentos.
- El documento compartido se exporta correctamente a DOCX.
- `cargo xtask ci` en verde.

## 15. Evolución posterior

Sin CRDT al principio, pero con protocolo y modelo preparados para migrar.

| Funcionalidad | Prioridad |
| --- | --- |
| Edición optimista sin esperar ACK | Alta |
| Undo/redo colaborativo | Alta |
| Formato enriquecido simultáneo | Alta |
| Tablas y estructuras avanzadas | Alta |
| Cursores y selecciones remotas | Media |
| Comentarios colaborativos | Media |
| Edición offline mediante CRDT | Media |
| Cliente web | Baja |

Migrar a CRDT no es cambiar el servidor: implica cambiar el modelo de posiciones, la representación del texto enriquecido y las reglas de sincronización. Es un proyecto técnico propio, no un cambio de librería.

## 16. Principios

1. **El servidor es autoritativo.** Nada modifica el estado compartido sin validación y persistencia.
2. **La colaboración opera sobre cambios documentales**, no sobre comandos de interfaz ni archivos DOCX completos.
3. **La integridad tiene prioridad sobre la inmediatez.** Edición optimista y estructuras complejas, después de demostrar la corrección del modelo básico.

**Primer trabajo concreto:** prueba de concepto en `collab-model` + `wordcraft-engine`: dos instancias independientes de `Session` reciben las mismas operaciones estructurales y terminan con documentos idénticos, sin WebSockets ni Docker. Si funciona con inserciones, borrados, divisiones de párrafo y Unicode, hay base para el servidor; si falla, se corrige el modelo antes de añadir capas.

## Referencias

- [Repositorio original](https://github.com/storytold/wordcraft) · [Arquitectura](../architecture.md) · [`Session::run`](../../crates/engine/src/session/run.rs) · [Primitivas de edición](../../crates/doc/src/edit.rs)

## Historial

- 2026-11-03 · v0.1 · Diseño inicial incorporado al repositorio junto con el scaffolding (C0).
- 2026-11-03 · C5.2 · Se documenta la implementación de capacidades, autorización ordenada con el actor, revocación de suscripciones y resincronización con snapshot/permisos actuales; sin cambio de versión de protocolo ni afirmación de preparación para producción.
- 2026-11-03 · C6.1 · Un único veredicto por `operation_id` (los rechazos definitivos se recuerdan). La revisión base de una edición es la que muestra el editor. Tras un reinicio, el historial del actor llega a antes del snapshot. Test de caos con tres clientes, proxy con cortes y duplicados, y reinicios. Sin cambio de versión de protocolo.
- 2026-11-03 · C5.5 · Pantalla de participantes en el editor, con `GET …/members`, `GET /api/v1/users?q=` y `GET /api/v1/me`. Cualquier usuario autenticado puede buscar personas por nombre (solo nombre e id): es el directorio de un servidor propio, no un servicio público.
- 2026-11-04 · C6.2/C6.4 · Backup SQLite exclusivamente en memoria, verificación en temporal privado y límites agregados de extracción; pings nativos y cancelación del transporte ante backpressure. Validación del homelab real pendiente.
