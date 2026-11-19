# NULO — material retirado, pendiente de tu revisión

Carpeta **temporal**. Guarda, sin reinterpretar, lo que se quitó o sustituyó en
la pasada de limpieza de documentación (rama `docs-sync-29.26.3`, 2026-11-19).

No forma parte del mapa de [`docs/README.md`](../docs/README.md), ninguna guía la
enlaza y `scripts/check-docs.sh` no la valida (solo recorre `README.md`,
`AGENTS.md`, `CHANGELOG.md` y `docs/**`). Es seguro copiarla fuera o borrar
`NULO/` entera cuando termines: no hay código ni configuración que dependa de
ella.

## Qué hay aquí

| Archivo | Contenido |
|---|---|
| `HANDOFF-previous-changes-29.20.0-29.26.3.md` | Las once secciones «Last change»/«Previous change» de `docs/HANDOFF.md` (29.20.0 a 29.26.3), copiadas literalmente de `61bbde2`. Se retiraron porque repetían `CHANGELOG.md`; el detalle por release sigue allí. |
| `removed-and-replaced.diff` | Diff completo `main..HEAD`: muestra, archivo por archivo, cada línea borrada (`-`) y su sustituto (`+`). Es el registro exacto de todo lo retirado, incluidas sustituciones que no tienen archivo propio. |

## Qué se retiró y dónde sigue vivo

- **Crónica de releases en HANDOFF**: solo historial; el detalle está en
  [`CHANGELOG.md`](../CHANGELOG.md) y el estado vigente en `docs/HANDOFF.md`.
- **Presupuestos de cartera de 29.20.0** (`docs/CLI.md`): sustituidos por la
  descripción actual de rondas, ventaja `txid`, 45 s y 20 s de red.
- **Mediciones y recetas fechadas**: números de 29.15.0 en `PRIVACY-TUNING` y el
  caso de la cartera de 239 transacciones, el límite de 20 txids de un proveedor
  concreto en `CLI`, y los comentarios de la receta de caché fría en
  `OPERATIONS`, que ahora remite a una instancia desechable.
- **Cifras antiguas de prefetch** (64 bloques / cola 1024) en `FEATURES` y
  `PRIVACY`: el código usa 256 por historial y una cola de 256..4096.
- **Ejemplo obsoleto de AUD-041** (1.7 negociando 1.6) en `AUDIT`.
- **Listas manuales de suites** en `README` y `GETTING-STARTED`: ahora se cita
  `tests/run-e2e.sh`.
- **Código**: se eliminó la constante `WALLET_BLOCK_BUDGET` de
  `crates/el-rawtx/src/lib.rs` (duplicaba a `WALLET_BLOCK_NET`, 20 s, y no se
  usaba) y se corrigió su comentario desactualizado. Sin cambio de
  comportamiento.

Si algo de aquí te sigue pareciendo útil, dímelo y lo reincorporo en el
documento que corresponda. Si no, borra la carpeta.
