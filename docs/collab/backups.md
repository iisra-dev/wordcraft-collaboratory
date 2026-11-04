# Backups cifrados y restauración

> **Estado:** C6.2, validación local (homelab real pendiente) · **Última actualización:** 2026-11-04 · Código: `apps/wordcraft-collab-server/src/backup.rs` · Tests: `apps/wordcraft-collab-server/tests/backup.rs`

El servidor hace copias cifradas de todo lo que necesita para volver a arrancar: la base de datos (usuarios, documentos, miembros, registro de operaciones y auditoría) y los snapshots de los documentos. Las copias se cifran con [age](https://age-encryption.org) para una o varias **claves públicas**. En el servidor solo están esas claves públicas. Para leer una copia hace falta la **clave privada**, que guardas tú fuera de esa máquina.

## Qué contiene una copia

Un archivo `wordcraft-<fecha UTC>.wcbackup.age`. Una vez descifrado es un zip con:

| Fichero | Qué es |
| --- | --- |
| `manifest.json` | Formato (`wordcraft-collab-backup/1`), fecha, versión del esquema, número de documentos y SHA-256 de cada fichero |
| `wordcraft.db` | Copia coherente en memoria con la API de backup de SQLite, tomada con el servidor en marcha |
| `documents/<uuid>/<revisión>.snapshot` | Todos los snapshots que nombra esa copia, comprobados contra el checksum de la base de datos |

No hace falta WordCraft para abrirla: `age -d -i key.txt copia.wcbackup.age > copia.zip`.

La copia de SQLite y el zip se crean en memoria y se cifran antes de escribirse: no se crea ningún fichero temporal de base de datos sin cifrar, ni siquiera en `BACKUP_DIR`, donde una herramienta de sincronización podría llevárselo antes de borrarlo. Hay un límite de 4 GiB para los datos expandidos (manifiesto incluido) y para el zip, y 16 MiB para el manifiesto. Se comprueba el tamaño de SQLite antes de copiarlo. La memoria máxima del proceso puede superar esos 4 GiB, porque conviven la base de datos, su serialización y el zip; no es un límite de RSS. Usa cifrado de disco/swap si también necesitas proteger la memoria paginada.

Cada copia cifrada se escribe en un temporal de nombre aleatorio y permisos privados, se sincroniza en disco y se publica sin reemplazar un archivo existente: una colisión de fechas falla sin perder la copia anterior. Así, un archivo de copia está completo o no existe.

## Configurarlo (Docker Compose)

1. Crea un par de claves en **otra** máquina (o guarda la privada fuera del servidor nada más crearla):
   ```bash
   age-keygen -o key.txt
   # Public key: age1...
   ```
   Guarda `key.txt` en un gestor de contraseñas o en un USB. **Sin ella las copias no se pueden leer.** Puedes poner varias claves públicas (por ejemplo, la tuya y la de otra persona de confianza) separadas por comas.
2. En `deploy/.env`:
   ```env
   BACKUP_RECIPIENTS=age1...
   BACKUP_EVERY=24h     # 12h, 90m…
   BACKUP_KEEP=14       # copias que se conservan; 0 = todas
   ```
3. `mkdir -p data backups && sudo chown 10001:10001 data backups`, y `docker compose up -d --build`. El servicio `backup` hace una copia al arrancar y después una cada `BACKUP_EVERY`. Usa la misma imagen que el servidor, no tiene red (`network_mode: none`) y borra solo sus propios archivos antiguos.
4. **Saca `deploy/backups/` de la máquina** con rsync, Syncthing, rclone o una copia del NAS. Un backup que vive en el mismo disco no protege contra que ese disco falle. Las copias están cifradas, así que el destino no necesita ser de confianza.

Sin Docker: `wordcraft-collab-server backup` hace una copia con las mismas variables (`DATABASE_URL`, `DOCUMENT_STORAGE`, `BACKUP_DIR`, `BACKUP_RECIPIENTS`, `BACKUP_KEEP`); `backup --every 24h` la repite.

## Comprobar que una copia se restaura

Hazlo de vez en cuando; una copia que no se ha probado no es una copia:

```bash
docker compose run --rm -T backup verify-backup latest --identity - < /ruta/a/key.txt
# /backups/wordcraft-….wcbackup.age restores: 3 documents, 9 snapshots, 1048576 bytes.
```

`--identity -` lee la clave privada por la entrada estándar: no se copia a ningún fichero del contenedor y no hace falta que su usuario (10001) pueda leer `key.txt`. `verify-backup` descifra la copia en un directorio temporal privado (creación atómica, permisos `0700` en Unix) y comprueba cada checksum. Rechaza rutas repetidas, archivos no enumerados y expansiones que excedan el límite total antes de extraer; también verifica el tamaño real de cada entrada. Después abre cada documento como lo haría el servidor (el snapshot más reciente que sea válido más las operaciones posteriores) y confirma que queda en la revisión que dice la base de datos. Al terminar borra el directorio temporal, tanto si la comprobación pasa como si falla. No toca los datos del servidor. En lugar de `latest` puedes pasar un fichero concreto.

## Restaurar

1. Para el servidor: `docker compose stop wordcraft-collab backup`.
2. Restaura en un directorio **nuevo** (`restore` se niega a escribir en uno que no esté vacío):
   ```bash
   mkdir restored && sudo chown 10001:10001 restored
   docker compose run --rm -T -v "$PWD/restored:/restored" backup \
     restore /backups/wordcraft-….wcbackup.age --identity - --to /restored < /ruta/a/key.txt
   ```
   Hace las mismas comprobaciones que `verify-backup` y deja `wordcraft.db` y `documents/` en `restored/`.
3. Cambia los datos: `mv data data.old && mv restored data && sudo chown -R 10001:10001 data`.
4. Arranca: `docker compose up -d`. Los clientes se reconectan solos y reciben el documento restaurado.

Si la copia es de un servidor más antiguo, el servidor nuevo aplica sus migraciones al abrirla, igual que en una actualización normal.

## Qué se pierde y qué no

- Se pierde lo editado después de la última copia. Con `BACKUP_EVERY=24h`, como mucho un día.
- Los tokens locales siguen funcionando, porque su hash está en la base de datos. Las sesiones OIDC dependen del proveedor, no de la copia.
- Los clientes que estaban conectados con ediciones pendientes las reenvían. Si se basaban en revisiones que la copia no tiene, el servidor las rechaza (`StaleRevision`) y el editor vuelve a cargar el documento.

## Por qué así

| Decisión | Motivo |
| --- | --- |
| age con claves X25519 | Herramienta estándar y auditada, sin contraseñas que guardar en el servidor; la copia se puede abrir sin WordCraft |
| Subcomando del propio servidor | La copia, la comprobación y la restauración usan el mismo código que el servidor para abrir documentos; está probado en `tests/backup.rs` |
| API de backup de SQLite a memoria | Transacción de lectura coherente sin parar los escritores en WAL; no deja SQLite sin cifrar en la carpeta que se sincroniza |
| Un fichero por copia, sin incrementales | Simple de copiar fuera y de restaurar; a escala de homelab el tamaño no es problema. Si crece, restic sobre `deploy/backups/` añade deduplicación sin cambiar nada aquí |

## Historial

- 2026-11-04 · Revisión de seguridad C6.2: SQLite copiado solo en memoria, verificación en temporal privado, presupuesto agregado de extracción, manifiesto acotado, publicación sin sobrescribir y regresión de intervalos Unicode de la CLI. La restauración en el homelab real sigue pendiente.
