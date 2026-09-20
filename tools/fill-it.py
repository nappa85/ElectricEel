#!/usr/bin/env python3
"""Fill translations/harbour-electric-eel_it.ts from the lupdate template.

One-shot authoring aid, not part of the build: tools/build-qm.sh
regenerates the template and merges it into per-language files (keeping
finished translations), then compiles .qm. Run this script once per new
language after adapting the STRINGS dict, then run build-qm.sh.
"""
import os
import sys
import xml.etree.ElementTree as ET

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TPL = os.path.join(REPO, "app", "translations", "harbour-electric-eel.ts")

LANG = sys.argv[1] if len(sys.argv) > 1 else "it"

STRINGS = {
    "Run": "Esegui",
    " (optional)": " (facoltativo)",
    "(not set)": "(non impostato)",
    "%1 • %2°C inside": "%1 • %2°C all'interno",
    "Climate on": "Clima acceso",
    "Climate off": "Clima spento",
    "%1% battery%2": "Batteria %1%%2",
    " • Charging": " • In carica",
    "Doors locked": "Portiere bloccate",
    "Doors unlocked": "Portiere sbloccate",
    "OK": "OK",
    "exit code %1": "codice di uscita %1",
    "Running %1...": "Esecuzione di %1...",
    "Attention": "Attenzione",
    "Honk Horn": "Clacson",
    "Flash Lights": "Lampeggio fari",
    "Wake Vehicle": "Risveglia veicolo",
    "Climate": "Clima",
    "Climate On": "Clima acceso",
    "Climate Off": "Clima spento",
    "Set Temperature": "Imposta temperatura",
    "Seat Heater": "Riscaldamento sedili",
    "Steering Wheel Heater": "Riscaldamento volante",
    "Auto Seat & Climate": "Sedili e clima automatici",
    "Add Preconditioning Schedule": "Aggiungi precondizionamento programmato",
    "Remove Preconditioning Schedule": "Rimuovi precondizionamento programmato",
    "Charging": "Ricarica",
    "Start Charging": "Avvia ricarica",
    "Stop Charging": "Interrompi ricarica",
    "Open Charge Port": "Apri sportello di ricarica",
    "Close Charge Port": "Chiudi sportello di ricarica",
    "Set Charge Limit": "Imposta limite di ricarica",
    "Set Charge Current": "Imposta corrente di ricarica",
    "Schedule Charging": "Programma ricarica",
    "Cancel Scheduled Charging": "Annulla ricarica programmata",
    "Add Charge Schedule": "Aggiungi programmazione ricarica",
    "Remove Charge Schedule": "Rimuovi programmazione ricarica",
    "Locks & Security": "Serrature e sicurezza",
    "Lock": "Blocca",
    "Unlock": "Sblocca",
    "Remote Start (Keyless Drive)": "Avvio remoto (guida senza chiave)",
    "Sentry Mode": "Modalità sentinella",
    "Valet Mode On": "Attiva modalità valet",
    "Valet Mode Off": "Disattiva modalità valet",
    "Guest Mode On": "Attiva modalità ospite",
    "Guest Mode Off": "Disattiva modalità ospite",
    "Erase Guest Data": "Cancella dati ospite",
    "Auto-Secure (Model X)": "Chiusura automatica (Model X)",
    "Trunk, Frunk & Windows": "Baule, cofano e finestrini",
    "Open Rear Trunk": "Apri baule posteriore",
    "Move Rear Trunk": "Muovi baule posteriore",
    "Close Rear Trunk": "Chiudi baule posteriore",
    "Open Front Trunk": "Apri cofano anteriore",
    "Open Tonneau (Cybertruck)": "Apri tonneau (Cybertruck)",
    "Close Tonneau (Cybertruck)": "Chiudi tonneau (Cybertruck)",
    "Stop Tonneau (Cybertruck)": "Ferma tonneau (Cybertruck)",
    "Vent Windows": "Socchiudi finestrini",
    "Close Windows": "Chiudi finestrini",
    "Media": "Multimediale",
    "Play / Pause": "Riproduci / Pausa",
    "Next Track": "Brano successivo",
    "Previous Track": "Brano precedente",
    "Next Favorite": "Preferito successivo",
    "Previous Favorite": "Preferito precedente",
    "Volume Up": "Alza volume",
    "Volume Down": "Abbassa volume",
    "Set Volume": "Imposta volume",
    "Software": "Software",
    "Start Software Update": "Avvia aggiornamento software",
    "Cancel Software Update": "Annulla aggiornamento software",
    "Keys": "Chiavi",
    "List Enrolled Keys": "Elenca chiavi registrate",
    "Add Key": "Aggiungi chiave",
    "Remove Key": "Rimuovi chiave",
    "Session Info": "Info sessione",
    "Diagnostics": "Diagnostica",
    "Ping Vehicle": "Ping veicolo",
    "Get Vehicle State": "Leggi stato veicolo",
    "Body Controller State": "Stato centralina carrozzeria",
    "Keep Accessory Power": "Mantieni alimentazione accessori",
    "Low Power Mode": "Modalità basso consumo",
    "The control core failed to start. Reinstall the app, then pull down to refresh.":
        "Il core di controllo non si è avviato. Reinstalla l'app, poi trascina in basso per aggiornare.",
    "The control core is too old to report its version. Reinstall the app (%1), then pull down to refresh.":
        "Il core di controllo è troppo vecchio per riportare la versione. Reinstalla l'app (%1), poi trascina in basso per aggiornare.",
    "Version mismatch: app %1, core %2. Reinstall the app, then pull down to refresh.":
        "Versioni non corrispondenti: app %1, core %2. Reinstalla l'app, poi trascina in basso per aggiornare.",
    "No VIN configured": "Nessun VIN configurato",
    "Key ready": "Chiave pronta",
    "No key - tap for Settings / Pairing": "Nessuna chiave - tocca per Impostazioni / Associazione",
    " • charging": " • in carica",
    "• %1°C": "• %1°C",
    "Updating...": "Aggiornamento...",
    "Status unavailable (%1). Vehicle may be asleep - try Wake Vehicle (Attention), then Refresh Status.":
        "Stato non disponibile (%1). Il veicolo potrebbe essere in standby - prova Risveglia veicolo (Attenzione), poi Aggiorna stato.",
    "Pull down to refresh status": "Trascina in basso per aggiornare lo stato",
    "Updated just now": "Aggiornato ora",
    "Updated %1m ago": "Aggiornato %1 min fa",
    "Categories": "Categorie",
    "Send Destination": "Invia destinazione",
    "Pair Vehicle": "Associa veicolo",
    "Settings": "Impostazioni",
    "Refresh": "Aggiorna",
    "Refresh Status": "Aggiorna stato",
    "Paste or type a destination first.": "Incolla o digita una destinazione.",
    "Checking...": "Verifica...",
    "Nothing to send.": "Niente da inviare.",
    "Cannot use this: %1": "Non utilizzabile: %1",
    "Coordinates %1, %2 — navigation will start there.":
        "Coordinate %1, %2 — la navigazione partirà da lì.",
    'Address "%1" — the car will look it up.':
        'Indirizzo "%1" — sarà cercato dall\u2019auto.',
    "Send failed: %1": "Invio fallito: %1",
    "Car Navigation": "Navigazione auto",
    "Destination": "Destinazione",
    "Paste address, coordinates, or map link": "Incolla indirizzo, coordinate o link mappa",
    "Preview": "Anteprima",
    "Send to Car": "Invia all'auto",
    "Notes": "Note",
    "This uses Bluetooth, like lock/unlock — the car must be in range, no internet needed on either side. Coordinates are sent exactly; addresses and links are looked up by the car itself, so unusual spellings may resolve differently than on your phone. From Android apps: copy the address or link, then paste it above.":
        "Usa il Bluetooth, come blocca/sblocca — l'auto deve essere a portata, senza internet da entrambe le parti. Le coordinate sono inviate esattamente; indirizzi e link sono cercati dall'auto stessa, quindi grafie insolite potrebbero risolversi diversamente che sul telefono. Dalle app Android: copia l'indirizzo o il link, poi incollalo qui sopra.",
    'Key generated. Tap "Pair with Vehicle", then tap your NFC card on the center console when prompted on the car\'s screen.':
        'Chiave generata. Tocca "Associa veicolo", poi appoggia la tessera NFC sulla console centrale quando richiesto sullo schermo dell\u2019auto.',
    "Key generation failed: %1": "Generazione chiave fallita: %1",
    "Paired.": "Associato.",
    "Pairing failed: %1": "Associazione fallita: %1",
    "Pairing & Keys": "Associazione e chiavi",
    "Set the VIN in Settings first. Then generate a key, then pair it with the car over BLE - you'll need to be next to the vehicle and tap the NFC card on the center console to approve.":
        "Imposta prima il VIN in Impostazioni. Poi genera una chiave e associala all'auto via BLE - devi essere vicino al veicolo e appoggiare la tessera NFC sulla console centrale per approvare.",
    "Phone key starting...": "Avvio chiave telefono...",
    "Generating...": "Generazione...",
    "Generate Key": "Genera chiave",
    "Waiting for NFC tap...": "In attesa del tocco NFC...",
    "Pair with Vehicle": "Associa veicolo",
    "Requesting pairing over BLE - approve on the car's touchscreen / NFC card now.":
        "Richiesta associazione via BLE - approva sul touchscreen dell'auto / tessera NFC ora.",
    "Enrolled Keys": "Chiavi registrate",
    "Loading current configuration...": "Caricamento configurazione...",
    "Key on file": "Chiave presente",
    "No key yet - use Pair Vehicle from the main menu":
        "Ancora nessuna chiave - usa Associa veicolo dal menu principale",
    "Saved": "Salvato",
    "Save failed: %1": "Salvataggio fallito: %1",
    "Vehicle VIN": "VIN veicolo",
    "17-character VIN": "VIN di 17 caratteri",
    "Front-page car model": "Modello auto in copertina",
    "Auto selects the model from the VIN": "Auto seleziona il modello dal VIN",
    "Key name": "Nome chiave",
    "harbour-electric-eel": "harbour-electric-eel",
    "Connect timeout": "Timeout connessione",
    " s": " s",
    "Command timeout": "Timeout comando",
    "Save": "Salva",
    "About": "Info",
    "App: ElectricEel %1   |   core: %2": "App: ElectricEel %1   |   core: %2",
    "(too old / unknown)": "(troppo vecchio / sconosciuto)",
    "The control core is too old to report a version - reinstall the app.":
        "Il core di controllo è troppo vecchio per riportarne la versione - reinstalla l'app.",
    "Version mismatch: core %1 vs app %2 - reinstall the app.":
        "Versioni non corrispondenti: core %1 vs app %2 - reinstalla l'app.",
    "Auto (from VIN)": "Auto (dal VIN)",
    "Model 3": "Model 3",
    "Model S": "Model S",
    "Model X": "Model X",
    "Model Y": "Model Y",
    "Cybertruck": "Cybertruck",
}


def main():
    tree = ET.parse(TPL)
    root = tree.getroot()
    root.set("language", LANG)
    missing = []
    for ctx in root.iter("context"):
        for m in ctx.iter("message"):
            src = m.findtext("source") or ""
            tr = m.find("translation")
            if tr is None:
                tr = ET.SubElement(m, "translation")
            if src in STRINGS:
                tr.text = STRINGS[src]
                if tr.get("type") == "unfinished":
                    del tr.attrib["type"]
            else:
                tr.set("type", "unfinished")
                missing.append(src)
    out = os.path.join(REPO, "app", "translations", f"harbour-electric-eel_{LANG}.ts")
    ET.indent(root)
    tree.write(out, encoding="utf-8", xml_declaration=True)
    print(f"wrote {out}")
    if missing:
        print(f"UNTRANSLATED ({len(missing)}):")
        for s in missing:
            print(f"  - {s}")
    else:
        print("all strings translated")


if __name__ == "__main__":
    sys.exit(main())
