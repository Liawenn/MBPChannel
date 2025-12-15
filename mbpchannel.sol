// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

contract MBPChannel is ReentrancyGuard {
    uint256 public constant MIN_DEPOSIT = 1 wei;
    uint256 public constant CHALLENGE_PERIOD = 2 minutes; 

    enum ChannelStatus { OPEN, CLOSING, CLOSED }

    struct Channel {
        address leader;
        uint256 totalBalance;
        ChannelStatus status;
        uint256 closingTime;
        // [新增] 记录当前提交状态的序列号，用于判断新旧
        uint256 bestNonce; 
        
        address[] participants;
        mapping(address => bool) isParticipant;
        mapping(address => uint256) finalBalances;
    }

    mapping(bytes32 => Channel) public channels;
    mapping(address => uint256) public pendingWithdrawals;

    event ChannelCreated(bytes32 indexed channelId, address indexed leader);
    event ParticipantJoined(bytes32 indexed channelId, address indexed participant, uint256 amount);
    // [修改] 事件包含 nonce，方便链下节点判断是否需要挑战
    event CloseRequested(bytes32 indexed channelId, uint256 nonce, uint256 closingTime);
    // [新增] 争议事件
    event ChannelDisputed(bytes32 indexed channelId, uint256 newNonce, address challenger);
    event ChannelSettled(bytes32 indexed channelId);
    event FundsWithdrawn(address indexed user, uint256 amount);

    modifier onlyStatus(bytes32 channelId, ChannelStatus _status) {
        require(channels[channelId].status == _status, "Invalid channel status");
        _;
    }

    modifier onlyParticipant(bytes32 channelId) {
        require(channels[channelId].isParticipant[msg.sender], "Not a participant");
        _;
    }

    // ... (createChannel 和 joinChannel 保持不变，省略以节省篇幅) ...
    function createChannel(bytes32 channelId) external payable {
        require(channels[channelId].leader == address(0), "Channel ID exists");
        require(msg.value >= MIN_DEPOSIT, "Deposit too low");
        Channel storage ch = channels[channelId];
        ch.leader = msg.sender;
        ch.status = ChannelStatus.OPEN;
        ch.participants.push(msg.sender);
        ch.isParticipant[msg.sender] = true;
        ch.totalBalance = msg.value;
        emit ChannelCreated(channelId, msg.sender);
        emit ParticipantJoined(channelId, msg.sender, msg.value);
    }

    function joinChannel(bytes32 channelId) external payable onlyStatus(channelId, ChannelStatus.OPEN) {
        require(msg.value > 0, "Deposit required");
        Channel storage ch = channels[channelId];
        require(!ch.isParticipant[msg.sender], "Already joined");
        ch.participants.push(msg.sender);
        ch.isParticipant[msg.sender] = true;
        ch.totalBalance += msg.value;
        emit ParticipantJoined(channelId, msg.sender, msg.value);
    }

    /**
     * @notice 提交关闭请求
     * @dev 增加了 nonce 参数
     */
    function initiateClose(
        bytes32 channelId,
        uint256 nonce, // [新增] 交易序列号
        address[] calldata recipients,
        uint256[] calldata amounts,
        bytes calldata signature
    ) external onlyStatus(channelId, ChannelStatus.OPEN) onlyParticipant(channelId) {
        Channel storage ch = channels[channelId];
        require(recipients.length == amounts.length, "Length mismatch");

        // 1. 验证资金守恒
        uint256 payoutTotal = 0;
        for (uint i = 0; i < amounts.length; i++) {
            payoutTotal += amounts[i];
        }
        require(payoutTotal <= ch.totalBalance, "State exceeds channel balance");

        // 2. 验证签名 (包含 nonce)
        _verifyConsensus(channelId, nonce, recipients, amounts, signature);

        // 3. 更新状态
        _updateChannelBalances(ch, recipients, amounts);
        
        ch.bestNonce = nonce; // 记录当前 Nonce
        ch.status = ChannelStatus.CLOSING;
        ch.closingTime = block.timestamp + CHALLENGE_PERIOD;

        emit CloseRequested(channelId, nonce, ch.closingTime);
    }

    /**
     * @notice [核心新增] 争议/挑战函数
     * @dev 如果有人提交了旧状态 (nonce N)，诚实节点可以提交 nonce N+k 的状态来覆盖它。
     */
    function disputeClose(
        bytes32 channelId,
        uint256 nonce,
        address[] calldata recipients,
        uint256[] calldata amounts,
        bytes calldata signature
    ) external onlyStatus(channelId, ChannelStatus.CLOSING) onlyParticipant(channelId) {
        Channel storage ch = channels[channelId];
        
        // 1. 挑战条件：提交的 nonce 必须比当前记录的 nonce 大
        require(nonce > ch.bestNonce, "Nonce not higher");
        
        // 2. 挑战期检查：必须在挑战期结束前提交
        // (注：有些协议允许挑战成功后重置挑战期，这里为了简化，不重置，只更新状态)
        require(block.timestamp < ch.closingTime, "Challenge period ended");

        // 3. 验证新状态的合法性
        _verifyConsensus(channelId, nonce, recipients, amounts, signature);

        // 4. 覆盖旧状态
        _updateChannelBalances(ch, recipients, amounts);
        ch.bestNonce = nonce;
        
        // 可选：惩罚机制 (Punishment)
        // 如果协议支持，可以在这里罚没上一个发起 initiateClose 的人的押金
        
        emit ChannelDisputed(channelId, nonce, msg.sender);
    }

    // 辅助函数：更新余额 (复用逻辑)
    function _updateChannelBalances(
        Channel storage ch, 
        address[] calldata recipients, 
        uint256[] calldata amounts
    ) internal {
        // 先清空旧映射 (Solidity 无法直接清空 mapping，只能覆盖)
        // 注意：在生产环境中，覆盖逻辑需要小心。
        // 这里假设 recipients 包含了所有需要分钱的人。
        for (uint i = 0; i < recipients.length; i++) {
            if (ch.isParticipant[recipients[i]]) {
                ch.finalBalances[recipients[i]] = amounts[i];
            }
        }
    }

    function finalizeClose(bytes32 channelId) external onlyStatus(channelId, ChannelStatus.CLOSING) {
        Channel storage ch = channels[channelId];
        require(block.timestamp >= ch.closingTime, "Challenge period not over");

        ch.status = ChannelStatus.CLOSED;

        for (uint i = 0; i < ch.participants.length; i++) {
            address p = ch.participants[i];
            uint256 amount = ch.finalBalances[p];
            if (amount > 0) {
                pendingWithdrawals[p] += amount;
                ch.finalBalances[p] = 0; 
            }
        }
        emit ChannelSettled(channelId);
    }

    function withdraw() external nonReentrant {
        uint256 amount = pendingWithdrawals[msg.sender];
        require(amount > 0, "No funds to withdraw");
        pendingWithdrawals[msg.sender] = 0;
        (bool success, ) = payable(msg.sender).call{value: amount}("");
        require(success, "Transfer failed");
        emit FundsWithdrawn(msg.sender, amount);
    }

    function _verifyConsensus(
        bytes32 channelId, 
        uint256 nonce, // [新增]
        address[] calldata recipients,
        uint256[] calldata amounts,
        bytes calldata signature
    ) internal view {
        Channel storage ch = channels[channelId];
        require(ch.leader != address(0), "Channel does not exist");

        // [修改] Hash 中加入 nonce，防重放攻击
        bytes32 stateHash = keccak256(abi.encode(
            channelId,
            nonce, 
            recipients,
            amounts
        ));

        require(stateHash != bytes32(0), "Hash error");
        require(signature.length > 0, "Sig missing");
        
        // 实际验证逻辑：
        // 验证该 signature 是否是由 Leader + 2/3 参与者聚合签名生成的
        // 这通常需要调用预编译合约 (Precompiled Contract) 进行 BLS Pairing Check
    }
}