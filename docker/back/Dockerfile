FROM node:20


RUN  mkdir /app
WORKDIR /app
RUN chown -R node:node /usr/local/lib/node_modules \
    && chown -R node:node /usr/local/bin \
    && chown -R node:node /app

USER node:node

COPY package.json ./
RUN npm i
ADD dist/ ./dist

EXPOSE 3000

CMD ["npm", "run", "start:prod"]