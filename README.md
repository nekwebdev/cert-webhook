Webhook for kubernetes to update linode nodebalancer when cert-manager updates or creates a new certificate. Comes with github actions to build and push to ghcr.io.

Example deployment:

```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: cert-hook
  namespace: cert-manager

---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: cert-hook-secrets-reader
rules:
- apiGroups: [""]
  resources: ["secrets"]
  verbs: ["get", "list"]

---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: cert-hook-secrets-reader
subjects:
- kind: ServiceAccount
  name: cert-hook
  namespace: cert-manager
roleRef:
  kind: ClusterRole
  name: cert-hook-secrets-reader
  apiGroup: rbac.authorization.k8s.io

---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: cert-hook
  namespace: cert-manager
spec:
  replicas: 1
  selector:
    matchLabels:
      app: cert-hook
  template:
    metadata:
      labels:
        app: cert-hook
    spec:
      serviceAccountName: cert-hook
      containers:
      - name: cert-hook
        image: ghcr.io/nekwebdev/cert-webhook:latest  # Replace with your container registry if you want
        resources:
          requests:
            memory: "30Mi"
            cpu: "10m"
          limits:
            memory: "50Mi"
            cpu: "50m"
        env:
        - name: LINODE_TOKEN
          valueFrom:
            secretKeyRef:
              name: linode-api-token-secret
              key: api-token
        - name: NODEBALANCER_ID
          value: "12345"  # Replace with your NodeBalancer ID
        - name: HTTPS_CONFIG_ID
          value: "12345"  # Replace with your NodeBalancer HTTPS Config ID
        - name: RUST_LOG
          value: "info"
        ports:
        - containerPort: 8080
        readinessProbe:
          httpGet:
            path: /health
            port: 8080
          initialDelaySeconds: 3
          periodSeconds: 10

---
apiVersion: v1
kind: Secret
metadata:
    name: linode-api-token-secret
    namespace: cert-manager
type: Opaque
stringData:
    api-token: "{{ linode_token }}" # Replace with your Linode API token

---
apiVersion: v1
kind: Service
metadata:
  name: cert-hook-service
  namespace: cert-manager
spec:
  selector:
    app: cert-hook
  ports:
  - port: 80
    targetPort: 8080
  type: ClusterIP
```

Example Certificate definition:

```yaml
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: wildcard-mydomain
  namespace: default
  annotations:
    cert-manager.io/post-issue-hook: "http://cert-hook-service.cert-manager.svc/update-nodebalancer-cert"
spec:
  secretName: wildcard-mydomain-tls
  dnsNames:
  - "*.mydomain.com"
  - "mydomain.com"
  issuerRef:
    name: letsencrypt-staging
    kind: ClusterIssuer
```
