import os
import re
import sqlite3
import yaml
import imaplib
import email
from email.header import decode_header
import zipfile
import io
import glob
import base64
from flask import Flask, render_template_string, request
import time

# Config
CONFIG_FILE = 'config.yaml'

def load_config():
    with open(CONFIG_FILE, 'r') as f:
        return yaml.safe_load(f)

config = load_config()

# Flask app
app = Flask(__name__)

HTML_TEMPLATE = """
<!DOCTYPE html>
<html>
<head>
    <title>Invoice Processor</title>
    <style>
        body { font-family: Arial, sans-serif; margin: 20px; background: #f5f5f5; }
        h1 { color: #333; }
        .btn { background: #4CAF50; color: white; padding: 10px 20px; border: none; cursor: pointer; font-size: 16px; }
        .btn:hover { background: #45a049; }
        table { border-collapse: collapse; width: 100%; background: white; margin-top: 20px; }
        th, td { border: 1px solid #ddd; padding: 12px; text-align: left; }
        th { background: #4CAF50; color: white; }
        tr:nth-child(even) { background: #f2f2f2; }
        .status { margin: 20px 0; padding: 15px; background: white; border-radius: 5px; }
    </style>
</head>
<body>
    <h1>Invoice Processor</h1>
    <form method="POST" action="/process">
        <button type="submit" class="btn">Process Now</button>
    </form>
    <div class="status">{{ status|safe }}</div>
    <h2>Invoices ({{ invoices|length }})</h2>
    <table>
        <tr>
            <th>Original Subject</th>
            <th>Vendor</th>
            <th>Invoice #</th>
            <th>Date</th>
            <th>Amount</th>
        </tr>
        {% for inv in invoices %}
        <tr>
            <td>{{ inv[5] or '' }}</td>
            <td>{{ inv[1] }}</td>
            <td>{{ inv[2] }}</td>
            <td>{{ inv[3] }}</td>
            <td>{{ "%.2f"|format(inv[4]) }}</td>
        </tr>
        {% endfor %}
    </table>
</body>
</html>
"""

def init_db():
    conn = sqlite3.connect('invoices.db')
    conn.execute('''CREATE TABLE IF NOT EXISTS invoices (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        vendor TEXT,
        invoice_number TEXT,
        invoice_date TEXT,
        amount REAL,
        original_subject TEXT,
        created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
    )''')
    conn.commit()
    conn.close()

def log(msg):
    with open('debug.log', 'a') as f:
        f.write(f"[{time.strftime('%Y-%m-%d %H:%M:%S')}] {msg}\n")

def extract_from_email(subject, body, msg=None):
    """Extract invoice data from email subject and body"""
    result = {'vendor': '', 'invoice_number': '', 'invoice_date': '', 'amount': 0.0}
    
    log(f"EXTRACT - Subject: {subject[:80]}")
    
    # Parse subject format: 900891335;TUCABLE SAS;TC561974;01;TUCABLE SAS;
    if subject:
        subj = subject.replace('\n', ' ').replace('\r', ' ')
        parts = [p.strip() for p in subj.split(';') if p.strip()]
        
        if len(parts) >= 3:
            result['invoice_number'] = parts[2]
            result['vendor'] = parts[1]
            log(f"EXTRACT - Vendor: {result['vendor']}, Invoice#: {result['invoice_number']}")
    
    # Parse body for date and amount
    if body:
        body_text = body.replace('\n', ' ').replace('\r', ' ')
        
        # Find date in body
        date_match = re.search(r'Fecha[^:]*(\d{1,2})[-/](\d{1,2})[-/](\d{2,4})', body_text, re.IGNORECASE)
        if date_match:
            result['invoice_date'] = f"{date_match.group(1)}-{date_match.group(2)}-{date_match.group(3)}"
            log(f"EXTRACT - Date from body: {result['invoice_date']}")
        
        # FALLBACK: If no date found, try email header
        if not result['invoice_date'] and msg:
            date_header = msg.get('Date') or ''
            header_date_match = re.search(r'(\d{1,2})\s+(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+(\d{4})', date_header, re.IGNORECASE)
            if header_date_match:
                day = header_date_match.group(1).zfill(2)
                month_map = {'jan': '01', 'feb': '02', 'mar': '03', 'apr': '04', 'may': '05', 'jun': '06',
                            'jul': '07', 'aug': '08', 'sep': '09', 'oct': '10', 'nov': '11', 'dec': '12'}
                month = month_map.get(header_date_match.group(2).lower()[:3], '01')
                year = header_date_match.group(3)
                result['invoice_date'] = f"{day}-{month}-{year}"
                log(f"EXTRACT - Date from header: {result['invoice_date']}")
        
        # Find amount
        amount_patterns = [
            r'VALOR\s+\$\s*([\d,]+(?:\.\d{2})?)',
            r'(?:Valor|Total)[:\s]+([\d.,]+)',
            r'\$\s*([\d,]+(?:\.\d{2})?)',
            r'Importe[:\s]+([\d.,]+)',
        ]
        
        for pattern in amount_patterns:
            amount_match = re.search(pattern, body_text, re.IGNORECASE)
            if amount_match:
                amt_str = amount_match.group(1)
                amt_str = amt_str.replace('.', '').replace(',', '.')
                try:
                    result['amount'] = float(amt_str)
                    log(f"EXTRACT - Amount: {result['amount']}")
                    break
                except:
                    pass
    
    log(f"EXTRACT - Final: {result}")
    return result

def process_emails():
    log("=== Starting email processing ===")
    
    cfg = config['imap']
    log(f"IMAP: host={cfg.get('host')}, user={cfg.get('user')}")
    
    try:
        mail = imaplib.IMAP4_SSL(cfg['host'])
        mail.login(cfg['user'], cfg['password'])
    except Exception as e:
        log(f"IMAP ERROR: {e}")
        return f"Error: {e}"
    
    for folder in ['INBOX', 'INBOX/facturas', 'facturas', 'Facturas']:
        try:
            mail.select(folder)
            log(f"Connected to: {folder}")
            break
        except:
            continue
    
    status, messages = mail.search(None, 'ALL')
    email_ids = messages[0].split()
    log(f"Found {len(email_ids)} emails")
    
    os.makedirs('invoices', exist_ok=True)
    
    processed = 0
    conn = sqlite3.connect('invoices.db')
    
    for email_id in email_ids:
        try:
            status, msg_data = mail.fetch(email_id, '(RFC822)')
            msg = email.message_from_bytes(msg_data[0][1])
            
            subject = ''
            if msg['Subject']:
                decoded = decode_header(msg['Subject'])
                subject = decoded[0][0] if decoded[0][0] else ''
                if isinstance(subject, bytes):
                    subject = subject.decode('utf-8', errors='ignore')
            
            body = ''
            if msg.is_multipart():
                for part in msg.walk():
                    if part.get_content_type() == 'text/plain':
                        payload = part.get_payload(decode=True)
                        body = payload.decode('utf-8', errors='ignore') if payload else ''
                        break
            else:
                payload = msg.get_payload(decode=True)
                body = payload.decode('utf-8', errors='ignore') if payload else ''
            
            # Extract from EMAIL - pass msg for header date fallback
            invoice_data = extract_from_email(subject, body, msg)
            
            # Check if already processed
            cur = conn.cursor()
            cur.execute("SELECT id FROM invoices WHERE invoice_number = ?", (invoice_data['invoice_number'],))
            if cur.fetchone():
                continue
            
            # Process ZIP attachments
            for part in msg.walk():
                if part.get_content_disposition() == 'attachment':
                    filename = part.get_filename()
                    if filename and filename.endswith('.zip'):
                        zip_data = part.get_payload(decode=True)
                        try:
                            with zipfile.ZipFile(io.BytesIO(zip_data)) as zf:
                                for name in zf.namelist():
                                    if name.endswith('.pdf'):
                                        zf.extract(name, 'invoices')
                                        if invoice_data['vendor'] and invoice_data['invoice_number']:
                                            old_path = f"invoices/{name}"
                                            date_str = invoice_data['invoice_date'].replace('-', '') if invoice_data['invoice_date'] else ''
                                            new_name = f"{invoice_data['vendor'].replace(' ', '_')}_{invoice_data['invoice_number']}_{date_str}_{invoice_data['amount']}.pdf"
                                            try:
                                                os.rename(old_path, f"invoices/{new_name}")
                                            except Exception as e:
                                                log(f"Rename error: {e}")
                        except Exception as e:
                            log(f"ZIP error: {e}")
            
            # Save to DB
            if invoice_data['invoice_number']:
                conn.execute(
                    "INSERT INTO invoices (vendor, invoice_number, invoice_date, amount, original_subject) VALUES (?, ?, ?, ?, ?)",
                    (invoice_data['vendor'], invoice_data['invoice_number'], invoice_data['invoice_date'], invoice_data['amount'], subject)
                )
                conn.commit()
                log(f"SAVED: {invoice_data['vendor']} - {invoice_data['invoice_number']} - {invoice_data['amount']}")
                processed += 1
        
        except Exception as e:
            log(f"Error: {e}")
            continue
    
    conn.close()
    mail.logout()
    log(f"=== DONE: {processed} invoices ===")
    return f"Processed {processed} invoices!"

@app.route('/')
def index():
    conn = sqlite3.connect('invoices.db')
    conn.row_factory = sqlite3.Row
    cur = conn.execute("SELECT * FROM invoices ORDER BY id DESC")
    invoices = cur.fetchall()
    conn.close()
    return render_template_string(HTML_TEMPLATE, status='', invoices=invoices)

@app.route('/process', methods=['POST'])
def process():
    result = process_emails()
    conn = sqlite3.connect('invoices.db')
    conn.row_factory = sqlite3.Row
    cur = conn.execute("SELECT * FROM invoices ORDER BY id DESC")
    invoices = cur.fetchall()
    conn.close()
    return render_template_string(HTML_TEMPLATE, status=result, invoices=invoices)

if __name__ == '__main__':
    init_db()
    log("=== Server started ===")
    app.run(host='0.0.0.0', port=5000, debug=False)
