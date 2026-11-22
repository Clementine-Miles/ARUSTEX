package notify

import (
	"context"
	"crypto/tls"
	"encoding/base64"
	"errors"
	"fmt"
	"mime"
	"net"
	"net/smtp"
	"strconv"
	"strings"
	"time"
)

// The dial and session timeouts bound connection setup and the complete SMTP
// conversation. A stalled peer otherwise blocks the serial dispatcher.
const (
	emailDialTimeout    = 10 * time.Second
	emailSessionTimeout = 45 * time.Second
)

// emailChannel delivers notifications over SMTP.
type emailChannel struct{}

func (emailChannel) Kind() string { return KindEmail }

// Use a generous default rate to avoid flooding recipients.
func (emailChannel) DefaultRatePerMin() int { return 60 }

// Mask the SMTP password. Connection and recipient fields remain editable.
func (emailChannel) SecretKeys() []string { return []string{"password"} }

// Host, port, and TLS determine where and how the password is sent. Changing
// any of them requires an explicit password decision.
func (emailChannel) DestinationKeys() []string { return []string{"host", "port", "tls"} }

func (emailChannel) Validate(cfg map[string]any) error {
	if cfgString(cfg, "host") == "" {
		return errors.New("缺少 SMTP 服务器地址")
	}
	port := cfgInt(cfg, "port")
	if port <= 0 || port > 65535 {
		return errors.New("SMTP 端口无效（应为 1-65535）")
	}
	if cfgString(cfg, "from") == "" {
		return errors.New("缺少发件人地址")
	}
	if len(cfgStrings(cfg, "to")) == 0 {
		return errors.New("至少需要一个收件人地址")
	}
	return nil
}

func (c emailChannel) Send(ctx context.Context, cfg map[string]any, m Message) (int, error) {
	if err := c.Validate(cfg); err != nil {
		return 0, Permanent(err)
	}
	host := cfgString(cfg, "host")
	port := cfgInt(cfg, "port")
	from := cfgString(cfg, "from")
	to := cfgStrings(cfg, "to")
	username := cfgString(cfg, "username")
	password := cfgString(cfg, "password")
	implicitTLS := cfgBool(cfg, "tls")

	msg, err := buildEmailMessage(from, to, m)
	if err != nil {
		return 0, Permanent(err)
	}

	addr := net.JoinHostPort(host, strconv.Itoa(port))
	client, err := emailDial(ctx, addr, host, implicitTLS)
	if err != nil {
		return 0, err
	}
	defer client.Close()

	// Upgrade to STARTTLS when the server advertises it.
	if !implicitTLS {
		if ok, _ := client.Extension("STARTTLS"); ok {
			if err := client.StartTLS(&tls.Config{ServerName: host, MinVersion: tls.VersionTLS12}); err != nil {
				return 0, fmt.Errorf("STARTTLS 失败: %w", err)
			}
		}
	}
	if username != "" {
		if err := client.Auth(smtp.PlainAuth("", username, password, host)); err != nil {
			// smtp.PlainAuth protects credentials on an unencrypted connection, with a
			// localhost exception. Give operators a helpful error when it refuses to send.
			if strings.Contains(err.Error(), "unencrypted connection") {
				return 0, Permanent(fmt.Errorf("拒发凭据：连接未加密。请启用 TLS，或改用 465 端口(隐式 TLS)，或把「启用 TLS」勾上 (%w)", err))
			}
			return 0, Permanent(fmt.Errorf("SMTP 认证失败: %w", err))
		}
	}
	if err := client.Mail(from); err != nil {
		return 0, smtpStageError(fmt.Sprintf("发件人 %s 被拒", from), err)
	}
	for _, rcpt := range to {
		if err := client.Rcpt(rcpt); err != nil {
			return 0, smtpStageError(fmt.Sprintf("收件人 %s 被拒", rcpt), err)
		}
	}
	w, err := client.Data()
	if err != nil {
		return 0, fmt.Errorf("SMTP DATA 失败: %w", err)
	}
	if _, err := w.Write([]byte(msg)); err != nil {
		return 0, fmt.Errorf("写入邮件正文失败: %w", err)
	}
	if err := w.Close(); err != nil {
		return 0, fmt.Errorf("提交邮件失败: %w", err)
	}
	// Ignore QUIT failures after the server accepts the message.
	_ = client.Quit()
	// The full HTML body is sent, so all items count as delivered.
	return len(m.Items), nil
}

// emailDial opens the SMTP connection.
//
// Implicit TLS starts encryption during dialing, as on port 465. Other SMTP
// modes connect first and may upgrade with STARTTLS, as on ports 25 and 587.
//
// Set the session deadline before handing the socket to net/smtp, which hides
// its connection. The deadline also covers the handshake. Apply the shared
// address guard on dial so SMTP follows the same direct connection policy and
// checks resolved IPs at connection time.
func emailDial(ctx context.Context, addr, host string, implicitTLS bool) (*smtp.Client, error) {
	d := &net.Dialer{Timeout: emailDialTimeout, Control: blockInternalDial}
	var conn net.Conn
	var err error
	if implicitTLS {
		conn, err = tls.DialWithDialer(d, "tcp", addr, &tls.Config{ServerName: host, MinVersion: tls.VersionTLS12})
	} else {
		conn, err = d.DialContext(ctx, "tcp", addr)
	}
	if err != nil {
		return nil, fmt.Errorf("连接 SMTP 服务器失败: %w", err)
	}
	_ = conn.SetDeadline(time.Now().Add(emailSessionTimeout))
	client, err := smtp.NewClient(conn, host)
	if err != nil {
		conn.Close()
		return nil, fmt.Errorf("SMTP 握手失败: %w", err)
	}
	return client, nil
}

// smtpStageError classifies an SMTP stage failure as retryable or permanent.
//
// SMTP 4xx replies are transient and merit a retry, including greylisting.
// SMTP 5xx replies represent permanent rejection.
//
// Missing reply codes are treated as retryable to preserve delivery after
// potentially transient failures.
func smtpStageError(what string, err error) error {
	code := smtpReplyCode(err.Error())
	if code >= 500 && code < 600 {
		return Permanent(fmt.Errorf("%s: %w", what, err))
	}
	return fmt.Errorf("%s: %w", what, err)
}

// smtpReplyCode extracts the leading three-digit reply code, or returns zero.
// net/smtp provides the reply as text such as "450 4.7.1 ...".
func smtpReplyCode(text string) int {
	if len(text) < 3 {
		return 0
	}
	n, err := strconv.Atoi(text[:3])
	if err != nil {
		return 0
	}
	return n
}

// buildEmailMessage assembles the RFC 5322 message.
//
// Base64 keeps HTML within SMTP's line limit and avoids lines beginning with
// a dot, which would require SMTP dot escaping.
func buildEmailMessage(from string, to []string, m Message) (string, error) {
	var b strings.Builder
	fmt.Fprintf(&b, "From: %s\r\n", from)
	fmt.Fprintf(&b, "To: %s\r\n", strings.Join(to, ", "))
	// Encode non-ASCII subject lines according to RFC 2047.
	fmt.Fprintf(&b, "Subject: %s\r\n", mime.QEncoding.Encode("utf-8", htmlTitle(m)))
	b.WriteString("MIME-Version: 1.0\r\n")
	b.WriteString("Content-Type: text/html; charset=\"UTF-8\"\r\n")
	b.WriteString("Content-Transfer-Encoding: base64\r\n")
	// Email can carry the complete HTML body without truncating the digest.
	b.WriteString("\r\n")
	encoded := base64.StdEncoding.EncodeToString([]byte(htmlBody(m, 0)))
	// Fold base64 at 76 characters as required by RFC 2045.
	for len(encoded) > 76 {
		b.WriteString(encoded[:76] + "\r\n")
		encoded = encoded[76:]
	}
	b.WriteString(encoded + "\r\n")
	return b.String(), nil
}
