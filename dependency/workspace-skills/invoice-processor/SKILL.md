---
name: invoice-processor
description: "Process invoices from emails, extract data using regex, display on web interface, upload to FTP"
metadata: {"nanobot":{"requires":{"bins":["python"],"env":[]},"always":false}}
---

# Invoice Processor

Process invoices from emails, extract data using regex and AI, display on web interface, and upload to FTP server.

## Quick Start

```bash
cd C:\Users\chack\.metis\workspace\email-app
python invoice_processor.py
```

Server runs at http://localhost:5000

## Features

1. **Email Processing** - Connect to IMAP, process emails with ZIP attachments
2. **Data Extraction** - Extract vendor, invoice#, date, amount from email subject/body
3. **AI Fallback** - Use OpenAI/MiniMax for missing data (optional)
4. **Web Interface** - View all invoices at http://localhost:5000
5. **FTP Upload** - Auto-upload PDFs to `/factura/YYYY/MM/`

## Data Extraction

### Email Subject Format
```
;INVOICE_NUM;TYPE;VENDOR;DATE;AMOUNT
```
- Invoice number: 3rd semicolon-separated value
- Vendor: 2nd value (contains SAS, LTDA, etc.)

### Email Body
- Date: Look for "Fecha", "Fecha de emisión" (DD-MM-YYYY)
- Amount: Look for "Valor", "Valor Total", "VALOR $"

### Fallback: Email Headers
- Date: Parse "Sent:" or "Date:" header if not found in body

## FTP Upload

After processing, PDFs are uploaded to:
- **Path:** `/factura/YYYY/MM/` (e.g., `/factura/2026/4/`)
- **Credentials:** See `credentials.md`

The upload:
1. Creates folder structure `/factura/YYYY/MM/`
2. Uploads all PDFs from `invoices/` folder
3. Logs upload status to `debug.log`

## Configuration (config.yaml)

```yaml
imap:
  host: imap.gmail.com
  user: email@gmail.com
  password: app_password

ftp:
  host: <ftp_host>
  user: <ftp_user>
  password: <ftp_password>
  base_path: /factura

minimax:
  api_key: your_api_key

openai:
  api_key: your_api_key  # Optional
```

## Dependencies

```bash
pip install flask pyyaml imap-tools requests openai pymupdf
```

## Files

- `invoice_processor.py` - Main Flask app
- `config.yaml` - Configuration
- `invoices/` - Extracted PDFs
- `invoices.db` - SQLite database
- `debug.log` - Processing logs
- `credentials.md` - API keys and FTP credentials
