# SMProbe v2 — BTM stability test protocol (macOS 27.2, 26B123)

**Chi esegue questo protocollo: TU, l'utente, a mano.** Ogni comando
privilegiato o interattivo è tuo: prompt di sistema, `sudo`, riavvio. La
sonda non fa NIENTE di privilegiato da sola: registra un LaunchDaemon via
`SMAppService` solo quando **tu** lanci l'app, e il demone risponde solo a
`ping`.

**Perché questo test.** La soluzione 1 di GP Bar (SMAppService + XPC) è
parcheggiata per quattro anomalie osservate su macOS 27.0.1 beta 26A434
(`docs/btm-27.0.1-beta.md`): **A** EPERM da app Accessory (evitata qui per
costruzione: nessun `LSUIElement`, policy Regular), **B** plist fuori firma
(evitata per costruzione: plist scritta a build time dentro la firma),
**C** registrazione che sparisce ore dopo (`NotFound` spontaneo), **D**
dopo `launchctl bootout` il job non riparte più nonostante BTM dica
`Enabled`. Su macOS 27.0 release è emerso pubblicamente un caso simile
(exelban/stats issue #3716: status `.notFound` + `register()` EPERM dopo
un auto-update dell'app). Questo protocollo misura **C** e **D** su macOS
27.2, con un'identità pulita (`dev.gpbar.smprobe`, mai condivisa con
GP Bar) e firma ad-hoc, come il baseline funzionante del 21/10.

**Log che il test produce** (tutti i passi li citano):

- `~/Library/Logs/smprobe.log` — una riga ogni 5 minuti dal sampler
  (timestamp, `SMAppService.status()` grezzo, esito del ping). È il
  **rilevatore dell'anomalia C**: nessuno deve guardare, la riga brutta si
  scrive da sola con il suo orario.
- `/Library/Logs/gp-bar/smprobe-helper.log` — log root del demone (una
  riga di startup + una per peer a ogni avvio). **Zero byte dopo un ping =
  launchd non ha mai avviato il job** = firma dell'anomalia D.

**Nota operativa sul demone**: è on-demand (niente KeepAlive) e **esce da
solo dopo 60 s senza connessioni aperte**. È voluto: così ogni ping
costringe launchd a riavviare il job da zero — la validazione continua
del meccanismo morto nell'anomalia D. Conseguenza: dopo un ping il
processo vive ~61 secondi; non cercarlo come residente.

---

## Passo 0 — build (una tantum, se non già fatto)

```sh
bash ~/Rust/gp-bar-solution1/smprobe/scripts/build-probe.sh
```

**GO:** il terminale di build finisce con `SMProbe.app is ready: ...` e
`OK: the daemon plist is sealed into the bundle signature`.
**NO-GO:** un errore qualsiasi — non procedere, incolla l'output.

## Passo 1 — installazione in un path stabile

```sh
cp -R ~/Rust/gp-bar-solution1/smprobe/build/SMProbe.app ~/Applications/
codesign -dv ~/Applications/SMProbe.app
```

**GO:** `codesign -dv` mostra `Identifier=dev.gpbar.smprobe` e
`Signature=adhoc`. (Il path stabile + la registrazione LaunchServices al
primo `open` sono le condizioni del 21/10; un binario nudo in una
cartella temp dà NotFound/EPERM.)
**NO-GO:** identifier diverso — ferma tutto, ricostruisci.

## Passo 2 — snapshot BTM PRIMA della registrazione

```sh
mkdir -p ~/BTM-TEST
sudo sfltool dumpbtm > ~/BTM-TEST/1-before.txt
grep -B2 -A10 -i smprobe ~/BTM-TEST/1-before.txt
```

**GO:** nessun record `dev.gpbar.smprobe` (grep vuoto), oppure al
massimo un record in stato non approvato. Conserva il file: è il
riferimento per capire se il record "sparisce" dopo.
**NO-GO:** esiste già un record attivo da test vecchi → fai prima il
Passo 7 (cleanup), poi riparti dal Passo 1.

## Passo 3 — registrazione (qui appare il prompt con Touch ID)

```sh
open ~/Applications/SMProbe.app
```

Il sampler parte (testa che sia vivo: `pgrep -fl
"SMProbe.app/Contents/MacOS/smprobe"`). Entro qualche secondo **deve
comparire il prompt di sistema** per autorizzare il LaunchDaemon →
approva con Touch ID. Poi verifica:

```sh
~/Applications/SMProbe.app/Contents/MacOS/smprobe status
~/Applications/SMProbe.app/Contents/MacOS/smprobe ping
sudo sfltool dumpbtm > ~/BTM-TEST/2-after-register.txt
grep -B2 -A10 -i smprobe ~/BTM-TEST/2-after-register.txt
tail -3 ~/Library/Logs/smprobe.log
```

**GO:**
- prompt apparso e approvato;
- `status` → **`1 (Enabled)`**;
- `ping` → **`ok uid=0 pid=NNN echo=ping`** (`uid=0` dimostra la catena:
  launchd ha avviato il demone come root);
- snapshot 2: record `dev.gpbar.smprobe` presente, disposition approvata;
- `smprobe.log`: righe `sampler=started`, `register=ok status=1 (Enabled)`
  e già un campione pulito `status=1(Enabled) ping=ok uid=0 ...`.

**NO-GO / dati da annotare se qualcosa stona:**
- nessun prompt + `status` resta `0` o `2`: registra il momento, il
  processo era attivo via `open`? (anomalia A in versione nuova);
- prompt ma `status` non diventa 1: annota il valore grezzo;
- `ping` fallisce: annota la riga di log e `sudo cat
  /Library/Logs/gp-bar/smprobe-helper.log`.

## Passo 4 — endurance, il rilevatore dell'anomalia C

Lascia il sampler girare per **ore** (mezza giornata o una notte è
ideale). Non serve fare nulla: ogni 5 minuti scrive una riga. Poi:

```sh
~/Applications/SMProbe.app/Contents/MacOS/smprobe log 1000
~/Applications/SMProbe.app/Contents/MacOS/smprobe log 1000 | grep -v "status=1(Enabled) ping=ok"
```

**GO:** il `grep -v` non stampa **nulla** (tutti i campioni puliti) e il
`log` mostra una riga ogni ~5 minuti senza buchi orari inspiegabili.
**NO-GO (anomalia C riprodotta):** qualsiasi riga con `status=3(NotFound)`
(o uno status grezzo sconosciuto, es. `status=4(Unknown(4))`) o
`ping=error:`. **Annota il timestamp esatto della prima riga brutta** e
confronta con lo snapshot 2: il record BTM c'era ed è sparito da solo.

Nota: una riga `ping=error` ISOLATA subito dopo un periodo di >60 s senza
ping può essere il demone che stava uscendo per idle proprio mentre il
campione arrivava (race del watchdog, attesa e documentata) — riprova
subito un `ping` a mano: se risponde ok, non è un'anomalia. Un `NotFound`
invece non è mai benigno.

## Passo 5 — riavvio

Riavvia il Mac, rientra nella sessione, **senza aprire l'app**, e da
Terminale:

```sh
~/Applications/SMProbe.app/Contents/MacOS/smprobe status
~/Applications/SMProbe.app/Contents/MacOS/smprobe ping
```

**GO:** `1 (Enabled)` e `ok uid=0 pid=NNN echo=ping` — il demone è
gestito da launchd, funziona senza l'app (il `uid=0` di un pid appena
nato dopo il boot è la prova che BTM ha ri-bootstrappato il job).
**NO-GO:** qualsiasi altra cosa. Fai lo snapshot:

```sh
~/Applications/SMProbe.app/Contents/MacOS/smprobe status
sudo launchctl print system/dev.gpbar.smprobe.helper
sudo sfltool dumpbtm > ~/BTM-TEST/3-after-reboot.txt
grep -B2 -A10 -i smprobe ~/BTM-TEST/3-after-reboot.txt
sudo cat /Library/Logs/gp-bar/smprobe-helper.log | tail -20
```

e **STOP**: quello è l'esito, documentalo (stato BTM dopo il reboot vs
snapshot 2). Se vuoi continuare l'endurance, riapri l'app con `open`.

## Passo 6 — il test dell'anomalia D (quello che ha ucciso la soluzione 1)

Con tutto funzionante (dopo il Passo 5, con l'app anche chiusa o aperta —
ma per ripetere la registrazione serve il sampler fermo, quindi):

```sh
pkill -f "SMProbe.app/Contents/MacOS/smprobe"   # ferma il sampler, se attivo
sudo launchctl bootout system/dev.gpbar.smprobe.helper
open ~/Applications/SMProbe.app                # ri-registrazione via sampler
```

Secondo l'osservazione del 22/10 la re-registrazione deve riuscire **senza
un nuovo prompt** (BTM ricorda l'approvazione). Il sampler riparte e
registra solo se serve. Poi, per qualche minuto, ping ripetuti:

```sh
for i in 1 2 3 4 5 6; do
  ~/Applications/SMProbe.app/Contents/MacOS/smprobe ping
  sleep 30
done
sudo launchctl print system/dev.gpbar.smprobe.helper
sudo tail -20 /Library/Logs/gp-bar/smprobe-helper.log
```

**GO (il caso che riabilita la soluzione 1):**
- `open` non mostra un nuovo prompt (o lo mostra e funziona — annota
  quale dei due: un nuovo prompt è già un dato, significa che BTM aveva
  perso l'approvazione);
- `ping` → `ok uid=0 pid=NNN echo=ping` **a ogni iterazione**: launchd ha
  riavviato il demone on-demand dopo il bootout;
- `launchctl print` trova il job, **non incollato**: subito dopo un ping
  mostra il demone attivo; dopo >60 s torna in attesa;
- il log del demone cresce di un blocco startup per ogni ping (es.
  `smprobe-helper: listening on dev.gpbar.smprobe.helper`).

**NO-GO (anomalia D riprodotta — la risposta che aspettiamo da questo
test):**
- `ping` → `error: the smprobe helper is unreachable...` (o timeout),
  mentre `status` continua a dire `1 (Enabled)`;
- `sudo launchctl print system/dev.gpbar.smprobe.helper` mostra il job
  "registrato" ma il demone non parte MAI: log del demone a **0 byte /
  nessun blocco startup** nuovo;
- eventuali `unregister`+`register` successivi non riportano il job in
  vita.

In quel caso documenta: orario del bootout, righe del sampler log, `sudo
launchctl print` completo, e rifai gli snapshot `sfltool dumpbtm`. È il
dato che serve per chiudere la questione BTM (bugreport Apple o cambio
di design).

## Passo 7 — pulizia (SEMPRE, a fine test)

```sh
~/Applications/SMProbe.app/Contents/MacOS/smprobe unregister
pkill -f "SMProbe.app/Contents/MacOS/smprobe"   # se il sampler è ancora attivo
rm -rf ~/Applications/SMProbe.app
sudo launchctl print system/dev.gpbar.smprobe.helper   # attendersi: could not find service
sudo sfltool dumpbtm > ~/BTM-TEST/4-after-cleanup.txt
grep -B2 -A10 -i smprobe ~/BTM-TEST/4-after-cleanup.txt  # attendersi: vuoto
```

**GO:** `unregister` ok, `launchctl print` non trova più nulla, snapshot 4
senza record. Il log del demone e `~/Library/Logs/smprobe.log` restano
come documentazione finché non li rimuovi a mano.

---

## Che cosa NON può rilevare questa sonda (limiti onesti)

- Non misura il **riavvio di una registrazione già approvata dopo
  l'aggiornamento del bundle** (il caso exelban/stats #3716 nasce da un
  auto-update): qui il bundle è immobile. Un test futuro potrà copiarlo.
- Il peer check (euid == proprietario del bundle) e la firma ad-hoc non
  sono il modello di produzione: l'esito del test dice se **BTM/launchd
  sono stabili**, non se la soluzione 1 è già sicura (quello richiede
  bundle non riscrivibile dall'utente e firma vera — review B1/B2).
- Un `ping=error` isolato al confine dei 60 s di idle del demone è una
  race nota (vedi Passo 4): si distinguono perché al retry il ping
  risponde ok.
- Lo scheduler del sampler dorme 5 minuti tra i campioni: un flip
  NotFound avvenuto e *revertito* dentro la stessa finestra di 5 minuti
  resta invisibile (il campione è puntuale, non continuo).
