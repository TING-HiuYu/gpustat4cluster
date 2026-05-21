This is the first release of gpustat4clustee. Although I have tesed it for many cases, there might be still some bugs. In the case of bugs occur, please create an issue. Thus I will be able to solve the issue.

For most deployments, install the server package on GPU nodes and the client package on user-facing nodes. Use KCP when UDP works; switch the client config to TCP if the network blocks UDP.
